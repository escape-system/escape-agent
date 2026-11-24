# 17 — Phase 1/2 interface contracts (agent coordination)

This file exists so parallel workstreams do not invent conflicting APIs. It is
authoritative for names and ownership; implementations may add methods but must
not rename these. If an implementation must deviate, it reports the deviation
in its handoff and updates this file.

## Landed APIs — source is normative after landing

These crates are implemented and green. The signatures below are the actual
public surface; `cargo doc -p <crate> --no-deps` is the reference.

### `oc-types`
- Wire modules: `session`, `message`, `inbox`, `permission`, `agent`, `file`,
  `config`, `payload` (+ existing `id`, `event`, `location`, `info`, `error`).
- `MessageBody::Assistant(Box<AssistantMessage>)` and
  `AssistantPart::Tool(Box<ToolPart>)` (boxed to keep the enum small; serde is
  unchanged on the wire).
- `SessionInfo.location` is `SessionLocation { directory, workspaceID? }`
  (exact `Session.Info`); `LocationInfo` also exists for envelopes.
- `InboxItem` carries `sessionID` and `time` in addition to the doc fields.
- `ConfigPatch.shell: Option<String>` cannot express unset; use a dedicated
  clear path later.
- Typed payloads implement `EventPayload` (`TYPE`, `DURABLE`, `VERSION`,
  `data()`, `into_envelope(location)`).
- Compaction lifecycle payloads are durable v1 events published after the
  matching commit: `SessionCompactionStarted`/`Ended`/`Failed`
  (`session.compaction.started|ended|failed`); ephemeral
  `session.compaction.delta` is not implemented, though the vendored UI still
  carries a dead handler for it.
- `Tokens.input` is **non-cached**; `Tokens::prompt_tokens()`
  (`input + cache.read + cache.write`) is the inclusive prompt size used for
  context/compaction math (a fully cached step can report `input == 0`).
- `Session.Info.tokens` is **cumulative** for the whole session;
  `session.usage.updated.tokens` is **per-step** current-context usage and is
  ephemeral (not durable, not replayed).
- `SessionExecutionStepLimit` (`session.execution.step_limit`, durable v1,
  `{sessionID, steps}`) plus the public helpers `STEP_LIMIT_PREFIX`,
  `step_limit_reason(n)` (`"step limit reached after {n} steps"`) and the strict
  `parse_step_limit(&str) -> Option<u64>` are re-exported from the crate root.
  The payload is the **qualifier of a succeeded terminal**: it is published
  after `session.execution.succeeded`, never as a terminal itself, and `steps`
  is the exhausted budget.

### `oc-store`
- `Store::{open, open_memory}`, `Store: Clone`; bodies are wire `Value`.
- Sessions: `create_session`, `get_session`, `list_sessions`, `patch_session`,
  `delete_session`; `SessionPatch` fields `title/agent/model/metadata/
  permissions/directory/path/revert/archived`.
- Messages: `append_message`, `get_message`, `list_messages`,
  `update_message`; `MessageQuery::new`.
- Inbox: `enqueue`, `list_inbox`, `promote_inbox`, `cancel_inbox`,
  `set_delivery`; duplicate id → `Conflict`.
- Events: `append_event` (persists + projects + dense seq in one transaction),
  `events_after`.
- `promote_inbox` allocates a fresh seq, deletes the inbox row, inserts the
  message, returns it; `None` for absent/control items.
- `create_session` auto-creates the owning `project` row.
- Memory API: `write_memory`, `search_memories`, `list_memories`,
  `forget_memory`, `touch_memories` (FTS5 backfill + triggers).
- Tree/subagent: `create_session_with_tree`, `set_session_policy`,
  `session_node`, `children`, `increment_child_count`.
- Agentic: `set_goal`/`get_goal`/`update_goal`/`list_goals`,
  `record_observation`/`settle_observation`/`list_observations`,
  `record_repair`/`list_repairs`.
- Forms: `create_form(NewForm)`, `list_forms(Option<&str>)`,
  `get_form(session_id, id)`, `reply_form(session_id, id, reply)`,
  `cancel_form`/`cancel_form_with_message`. Duplicate id → `Conflict`, a
  settled form → `Conflict`, missing session → `NotFound`.
- Saved permissions: `save_permission_rule`, `list_saved_permissions`,
  `update_saved_permission`, `delete_saved_permission`.
- Instructions: `instruction_entries`, `set_instruction_entry` (the store
  deletes the row when the value is `None`).
- Session maintenance: `message_seq`, `truncate_session(session_id, seq)`
  (deletes messages and inbox rows at/after `seq` in one transaction).
  `time_viewed` is written only by the `session.viewed` projector
  (`append_event`), which keeps it monotonic.
- Lane history: `save_lane_ticket(&Value)`, `lane_tickets(limit)`,
  `lane_tickets_all()` (no page limit, `queued` ascending) for boot restore.
- Projects: `list_projects(archived)`, `get_project`, `update_project`.
- Credentials: `create_credential(NewCredential)`, `list_credentials()`,
  `get_credential(id)`, `update_credential(id, CredentialPatch)`,
  `delete_credential(id)`, `activate_credential(id) -> Result<Value,
  StoreError>`. `NewCredential`/`CredentialPatch` are public (serde camelCase).
  Value must be `{type: "oauth"|"key"|"external", …}`; at most one active
  credential per integration, cleared atomically in the same transaction.
- Worktrees: `list_worktrees(project_id: Option<&str>)`,
  `create_worktree(NewWorktree)`, `get_worktree(id)`, `delete_worktree(id)`.
  `NewWorktree` is public. Rows emit only `Worktree.Info = {directory}`;
  `(project_id, directory)` duplicates are a `Conflict`.
- Schedules: `create_schedule(NewSchedule)`, `list_schedules(Option<&str>)`,
  `get_schedule(id)`, `update_schedule(id, SchedulePatch)`,
  `delete_schedule(id)`, `pause_schedule(id)`, `resume_schedule(id)`,
  `due_schedules(now_ms, limit)`, `mark_schedule_fired(id, ScheduleOutcome,
  next_due_at, run_count)`. Public types: `ScheduleWhen` (tagged
  `in|at|every|cron` + optional `tz`), `ScheduleAction` (`prompt`),
  `ScheduleEnds` (`after?|at?|times?`), `ScheduleOutcome`
  (`fired|late|skipped|disabled|failed`), `NewSchedule`, `SchedulePatch`, and
  `duration_ms(&str)`. `ScheduleWhen::first_due`/`next_after` own the timing
  math: relative triggers use epoch arithmetic, `at` accepts RFC 3339 or epoch
  ms, `every` is ≥ 1 minute and keeps its phase, cron is five-field. Zones are
  `UTC` or numeric `±HH:MM`; IANA names are `Invalid` (never coerced to UTC).
- Snapshots: `NewSnapshot` and `SnapshotRow` are public. Methods:
  `insert_snapshot(NewSnapshot)`, `snapshots_after(session_id, seq)`,
  `earliest_snapshots_after(session_id, seq)` (one row per path, minimum
  `seq`/`id`) and `delete_snapshots_after(session_id, seq)`. All boundaries are
  inclusive (`seq >= boundary`) because a pre-write capture belongs to the
  message it precedes. Rows carry `message_id`, `tool_call_id`, absolute `path`,
  `existed`, `skipped`, optional pre-image `content`, and `time_created`.

### `oc-config`
- `ConfigService::load(LoadOptions { directory, env })`; `entries()`
  (`ConfigEntry`, use `to_value()` for the wire), `merged()`, `agents()`,
  `agent(id)`, `evaluate(agent, action, resource)`, `skills_roots()`,
  `mcp_servers()`, `shell()`, `model()`, `diagnostics()`, `default_agent()`.
- Skills: `skills()` and `skill(id)` are the load-time global snapshot;
  `rescan_skills()` re-walks the same roots without updating it. Turns and the
  `skill`/`skill_write`/`skill_lint` tools resolve through
  `skills_for(directory) -> Arc<Vec<SkillInfo>>`, which scopes discovery to the
  session directory: project-scope roots outside it are dropped and its own
  `.opencode` ancestors are scanned instead, while global config roots,
  `skills[]` directories and the per-user `.claude`/`.agents` roots stay
  visible. An empty `directory` returns the snapshot; a relative one resolves
  against the load directory. The result is cached per directory until
  `invalidate_skills()`, which keeps the snapshot and drops the per-directory
  entries after a `SKILL.md` write.
- Instructions: `instructions()` is the load-time snapshot (the walk up from the
  `ConfigService::load` directory, outermost first). Turns resolve through
  `instructions_for(directory) -> Arc<Vec<InstructionFile>>`, which re-walks
  that directory's ancestor chain for `AGENTS.md`, `CLAUDE.md` and
  `.opencode/AGENTS.md`, outermost (most-shared) first, so a session never
  inherits the server repository's files. An empty `directory` returns the
  snapshot; a relative one resolves against the load directory. The result is
  cached per directory until `invalidate_instructions()`, which keeps the
  snapshot and drops the per-directory entries; edited contents are re-read
  every turn, so only added or removed files need the invalidation.
- Malformed files are diagnostics, not load failures.
- `serde_json` `preserve_order` is enabled by this crate.

### `oc-llm`
- `Catalog::{embedded, from_snapshot, from_snapshot_with_config, apply_config,
  models, providers, resolve, upsert_provider, upsert_provider_models}`;
  embedded upstream snapshot (7.9 MB).
- `LlmClient::{new, stream, http_client, rate_limits, record_rate_limit,
  with_hook, set_hook, hook, with_retry_policy, with_timeouts}` and
  `SharedLlmClient::{new, http_client, rate_limits, set_hook, hook,
  resolve_model, dynamic_models}`. There is no `new_with_hook`: `with_hook` is
  the builder on `LlmClient`, `set_hook` the setter on `SharedLlmClient`.
- `LlmTransportHook` (`handles`, `stream -> HookStreamFuture`,
  `resolve_model`, `dynamic_models`) in `hook.rs` lets an OAuth gateway own
  transport while the shared client keeps one HTTP pool and rate-limit log.
- `quota`: `QuotaCapability`, `provider_quota_capability`,
  `ProviderRateLimit`, `extract_rate_limit`, `ProviderQuotaTarget`,
  `QuotaWindow`, `ProviderBalanceSummary`, `ProviderApiSpend`,
  `ProviderQuotaProbeResult`, `fetch_provider_quota`,
  `fetch_provider_quotas`, `parse_reset_timestamp` are re-exported at the
  crate root; `parse_rfc3339_ms`, `default_http_client` and `now_epoch_ms` are
  public but only as `oc_llm::quota::*`.
- `LlmEvent`/`LlmMessage`/`ToolDef`/`LlmOptions`/`Usage` in `event.rs`;
  `Protocol::Unsupported` for providers not yet implemented.
- Retries 2s→10s, 10 attempts, jitter ±25% on the exponential term only;
  `RetryPolicy::max_retry_after` defaults to **60 s** (was 15 min). The
  provider's `retry-after` is a documented **floor**:
  `delay() = max(backoff, min(hint, max_retry_after))` with a final
  `.min(max(max, max_retry_after))` clamp (`crates/oc-llm/src/retry.rs:81-82`),
  so jitter can spread our own backoff but never shortens the wait the provider
  asked for. The outer clamp never binds with the shipped defaults
  (`max` 10 s, `max_retry_after` 60 s): the inner term is already ≤ 60 s, so it
  is a defensive ceiling, not a behaviour. Mid-stream failures emit
  `ProviderError` (no replay).
- `oc_llm::parse_retry_after(&str) -> Option<Duration>` accepts `delta-seconds`
  (fractional tolerated for test servers) and IMF-fixdate. It never panics:
  unparsable, negative, non-finite or out-of-range input returns `None`, so the
  caller falls back to exponential backoff. A past HTTP-date is `Some(ZERO)`
  ("retry now"); a hint beyond the cap is clamped by the policy, not here.
  Signature unchanged; `LlmError` stays `Debug`-only, so nothing new is
  serialized.

### `oc-tools`
- `Tool` trait (async, `name/description/schema/execute`), `ToolContext`,
  `ToolResult`, `ToolError`, `ToolRegistry::with_defaults` + sorted
  `definitions()`. `ToolContext` carries `session_id`, `directory`, `cwd` and
  an optional `cancel: Option<CancellationToken>`; the runtime cancels that
  token when the execution is interrupted, and tools that spawn detached
  helpers (the foreground subagent wait) use it to stop them instead of
  orphaning work when the tool future is dropped. Built-ins: `code_map`, `read`,
  `write`, `edit`, `symbol_edit`, `patch`, `glob`, `grep`, `shell`, `todowrite`,
  `webfetch`, `websearch`; no permission/expectation logic (the runtime adds it).

### `oc-mcp`
- `McpConfig::from_value`, `McpClient::{connect, list_tools, call_tool,
  read_resource, get_prompt, ping, shutdown}`, `McpTool` (schema already
  patched with `expectation`, execute strips it), `map_call_result`,
  `sanitize/effective_name`, `McpStatus`.
- Transports: stdio + streamable HTTP (+ external `Transport` trait for future
  WS attach). Legacy HTTP+SSE is not implemented. OAuth PKCE helpers exist;
  the host supplies the browser callback and injects credentials.

### `oc-plugin`
- `Plugin` trait, `PluginRegistry` (tiers/order/duplicates/guards/panic
  catch), `Domain`, `Hook` + `HookEvent`/`TransformDoc` JSON envelopes,
  `PluginInfo`, `RpcNamespace`, `plugin_inventory`.
- `Sidecar` speaks newline JSON-RPC over stdio, 8 MiB frames, 5 s request
  timeout, crash detection, `HostServices` callbacks. Shutdown kills only the
  direct child (`kill_on_drop`); grandchildren are not tracked. Fake sidecar
  binary for tests. Unsupported live-object hooks (`aisdk.*`, `experimental.ws.*`)
  are rejected at load.

### `oc-observer`
- `ObserverEngine`, `ObserverModel`, `RepairExecutor`, `ObserverNotifier`,
  `ObserverNotice`, `ObserverRun`, `RepairOutcome`, `SkipReason`,
  `TargetBudget`; `GoalPolicy`/`GoalSnapshot`/`apply_verdict`.
- `ObserverConfig` gains `repair_ack_timeout_ms: i64` (default `5000`; `0` =
  wait forever). It is a **configuration-only** field: `#[serde(default)]` on
  the struct, never wire-serialized by the server, so it is omitted rather
  than sent as `null`.
- `RepairExecutor::apply` signature is unchanged
  (`fn apply<'a>(&'a self, verdict: &'a ObserverVerdict) -> BoxFuture<'a, Result<RepairOutcome, ObserverError>>`)
  but its contract is now normative and documented on the trait: resolve only
  when the outcome is known; `applied = true` means performed; `applied =
  false` plus `detail` is a refusal (unknown model, nothing to cancel,
  unimplemented kind); `Err(Executor)` is failed/unimplemented. The engine
  bounds the wait by `repair_ack_timeout_ms` and treats **no answer as
  failure, never success**. A host that queues and resolves immediately can no
  longer publish a success it cannot back up.
- `ObserverRun` records a refusal as `Rejected` and an unconfirmed repair as
  `Unavailable`. Neither is serialized, and no wire field changed; the effect
  is that repair rows flip from `ok` to `error` for both, which is the intended
  truth. A refusal stays charged to the budget (an impossible repair must
  escalate instead of being re-proposed every tick) but clears the compaction
  cooldown; an unconfirmed one returns its reservation.

### `oc-runtime`
- Primitives: `Expectation::{normalize,derived}`, `inject`, `extract`,
  `InstructionSet`/`render`/`hash_text`, `tokens::*`, compaction template.
- Session engine: `Runtime::{new,new_with_model_client,start,notify,interrupt,
  wait_idle,active,shutdown}`, `RuntimeConfig`, `EventSink`,
  `PermissionHandler`, `PermissionAsk/Decision`, `ModelClient`,
  `LlmClientAdapter`, `prompt::{default_system_prompt,MAX_STEPS_PROMPT}`.
- Observer/memory/subagent extensions: `ObserverBinding` + `RepairChannel` +
  `StoreGoalStore`, `MemoryService` attachment, `SubagentTools`, digest.
- Host tool source: `Runtime::add_tool_provider(Arc<dyn ToolProvider>)`; the
  turn loop calls every provider once per execution and merges the result
  after `RuntimeConfig::tools` and before the runtime's own tool sets, so a
  provider can shadow a built-in by name.
- Runtime-owned tool sets, merged into every execution:
  - `GoalTools::new(runtime).registry()` → `goal_set`, `goal_update`,
    `goal_complete`, `goal_status`; `goal_complete` records evidence and bumps
    rounds, it never sets `done` (the observer owns that).
  - `SkillTools::new(runtime).registry()` → `skill`, `skill_write`,
    `skill_lint`; `skill_write` targets `<scope>/.opencode/skill/<id>/SKILL.md`
    or the global skill dir and marks the file `metadata.opencode/agent: true`.
  - `LaneTools::new(runtime).registry()` → `lane` plus a `shell` replacement
    (`ShellLaneTool`) that classifies cargo/test commands into lanes and
    delegates everything else to `oc_tools::ShellTool`.
  - `BoxTools::new(runtime).registry()` → `box_list`, `box_read`, `box_write`,
    `box_edit`, `box_search`, `box_meta`, `box_delete`, `compact_context`
    (`docs/25-context-boxes.md`).
    `box_read`/`box_list`/`box_search` map to `box:read`,
    `box_write`/`box_edit`/`box_meta`/`box_delete` to `box:write` with the box
    key as the resource; `compact_context` maps to action `compaction`,
    resource `*`. `compact_context(note)` always targets
    the CALLER's own session, never a supplied id. It never compacts inline:
    it commits an inbox row (`kind = "compaction"`, `delivery = Queue`,
    `reason = "manual"`), notifies the actor and returns `scheduled`; the
    compaction runs at the next turn boundary. Agents are expected to compact
    their own session around 60-70% of the model context window. `explore`
    agents do not get the tool (their `deny("*","*")` covers action
    `compaction`).
- `CompactionConfig` (`crate::compaction`) is the runtime view of the merged
  `compaction` document: `{ auto, keep_tokens, buffer, ceiling_tokens }`.
  `auto` defaults to `false`; the threshold path is nevertheless **armed** when
  `auto` is true **or** `ceiling_tokens` is set, so an explicit ceiling is a
  hard guard on its own. `CompactionConfig::from_merged(&Value)` is the only
  constructor; `Runtime::compaction_config()` calls it.
- `tokens::ceiling(context_window, configured) -> u64` computes
  `context_window − max(15%, 16k)`; a configured `compaction.ceiling_tokens`
  may only lower it (`min`). The former hard-coded 150k cap is gone.
- `tokens::effective_buffer(requested, context_window) -> u64` returns
  `requested` when non-zero and `max(8_000, context_window / 10)` otherwise;
  `tokens::trigger(ceiling, context_window, buffer) -> u64` is
  `ceiling − effective_buffer(...)`, saturating at 0. The turn loop compares
  the estimate against `trigger`, never against the ceiling directly.
- `Runtime::context_pressure(session)` (crate-private) returns
  `Result<Option<ContextPressure>, RuntimeError>` with
  `ContextPressure { estimated, ceiling, trigger }` — the same
  anchor-aware estimate and the same `tokens::ceiling`/`tokens::trigger`
  values the turn loop uses, so observers and compaction use the same
  formulas; the loop snapshots its compaction config once at turn start, so
  only a mid-turn config reload can make the two differ.
  `None` for an unknown session, an unresolvable model, or a window of 0.
  `Runtime::advise_context_pressure(&[String])` (crate-private, host-side, in
  `observer/advice.rs`) is the adviser built on it; thresholds and cooldown
  are specified in `docs/08`. It skips entirely when the runtime already
  protects the session (`compaction.auto` on **or** `compaction.ceiling_tokens`
  set) or when no ceiling can be computed.
- `observer.advised` is a durable v1 event
  `{sessionID, estimateTokens, ceilingTokens, triggerTokens}` published by that
  adviser next to its durable `observer` inbox row. Its payload type
  (`ObserverAdvised`) is **runtime-local** (`oc-runtime/src/observer/advice.rs`)
  and is **not** declared in `oc-types`; `oc-types` owns only the inbox kind
  (`InboxKind::Observer`).
- Turn settlement (`turn::settle`, crate-private): the internal
  `ExecutionOutcome` has four variants — `Succeeded`,
  `StepLimited { steps }`, `Failed(StructuredError)` and
  `Interrupted(InterruptReason)`. `StepLimited` is the internal variant
  (`StepLimited { steps }`), stored as `idle_outcome = "step_limit"` plus its
  reason, and mapped to the wire terminal `session.execution.succeeded`; the
  `session.execution.step_limit {sessionID, steps}` event follows it, never as
  a terminal. `SessionOutcome` stays exactly
  `succeeded|failed|interrupted`: `project_session` (list, get, export `info`)
  omits the key for any stored value outside the enum — never `null`, never
  remapped — so the three known literals pass through.
- Consuming an outcome: `succeeded` alone does not mean the turn completed.
  Check for the `session.execution.step_limit` qualifier (or the store-level
  `idle_reason` grammar, `docs/03-storage.md`). A parent subagent envelope
  for a step-limited child keeps `state: "completed"` but adds
  `metadata.stepLimit = true`, `metadata.steps = N` and one trailing line
  `[step budget exhausted after N steps; result may be incomplete]`.
- `InstructionContext.boxes: Option<&[Value]>` feeds `SourceKind::Boxes`
  (`oc/boxes`), ordered after `oc/memory` and before `oc/goal`. Like `oc/goal`
  it is pinned per actor epoch, so a mid-epoch box change is delivered by
  `InstructionSet::epoch_supplement` and only a new epoch rewrites the prefix.
- `Renderers`: `context_box::render_box_index(root_id, rows, limit)` renders
  key/kind/size/description only (never content); `BOX_INDEX_MAX = 100`,
  `MAX_BOX_WRITE_CHARS = 60_000`, `BOX_READ_CHUNK = 20_000`.
- `Runtime::with_lanes(Arc<LaneService>)` attaches the scheduler; `lanes()`
  returns it.
- `ProcessLaneRunner` implements `LaneRunner`: `new(tokens)`, `unbound()` +
  `bind(&service)`, `for_service(&service)`. It spawns the platform shell in
  `ticket.cwd`, shares one temp file for stdout+stderr under
  `<temp>/opencode-lanes`, and returns `RunnerResult::output_path`.
- Pre-write snapshots: the turn loop calls the crate-private
  `crate::snapshot::capture_before_tool(runtime, session_id, message_id,
  tool_name, tool_call_id, input, directory)` immediately before
  `tool.execute`. It captures only `write`/`edit` (`input.path`) and `patch`
  (`input.patchText` targets), resolves each path against the session directory
  (rejecting lexical and symlink escapes), records missing files with
  `existed=false` (so a revert deletes them), marks unreadable, non-regular or
  over-5-MiB (`CONTENT_CAP`) files as `skipped`, and logs/ignores any store
  failure — a tool call never fails because a snapshot failed. Snapshot ids are
  `snap_…`.

### `oc-subagents`
- `SessionNode`, `Role`, `NodeState`, `Limits` (defaults depth 3, children 8,
  siblings 20 messages/min, 20 writes/min), `Policy::{spawn,can_message,
  can_write,resolve_model}`, `TreeLookup`/`SessionTree`, `SpawnBudget`,
  `SiblingBudget`, `WriteBudget::try_write(now_ms) -> Result<(), BudgetError>`
  (60 s fixed window per sender, in memory), typed errors.
  `Limits::unrestricted()` sets every cap to `u32::MAX`; a configured
  `max_writes_per_min: 0` denies every write instead of meaning unlimited.
  The runtime reads `subagents.siblings.max_writes_per_min` (default 20) and
  refuses with `BudgetError: rate limit reached (max {N}/min); retry after
  {ms} ms` before any enqueue or interrupt.
- Coordinator scope = parent + siblings + their descendants; siblings can talk
  only when enabled; writes are descendants/self/scope.

### `oc-lanes`
- `classify(command,cwd) -> Option<LaneRequest>` for cargo/rustc/node/go/
  pytest/dotnet/gradle/mvn; workspace-aware lane keys.
- `LaneService::{enqueue,enqueue_detailed,status,ticket,cancel,reorder,wait,
  shutdown}` with priority FIFO per lane, semaphores, `LaneRunner`/`LaneEvents`/
  `Clock` traits; `tokens()` exposes the cancellation table for a runner.
  `enqueue` returns a `TicketId`; `enqueue_detailed` returns
  `EnqueueResult { id, subscribed }`.
- `LaneService::unsubscribe(id, session) -> Result<bool, LaneError>` drops one
  session's interest in a coalesced ticket (clearing the owner slot when it owns
  the ticket) and returns whether the run is still alive. The run survives while
  an owner or one subscriber remains, so a coalesced subscriber's timeout never
  cancels shared work; only when nobody is left is it cancelled like `cancel`.
  A terminal ticket returns `false` and is never reopened. Runtime-internal, no
  route exposes it.
- `LaneService::restore(config, runner, events, clock, source) ->
  Result<LaneService, RestoreError>` rebuilds the queue through
  `LaneTicketSource::load() -> BoxFuture<'_, Result<Vec<TicketRecord>,
  RestoreError>>`. `RestoreError` is boot-only, never on the wire, and separate
  from `LaneError` (`Source(String)`, `Ticket { id, reason }`); `LaneError` has
  no restore variant, so every route-level match stays exhaustive.
  `TicketRecord` is the persisted subset `{id, lane, command, cwd, priority,
  ownerSession?, state, queuedMs, startedMs?, finishedMs?, outputPath?}`; it
  carries no `tree_rev` and no `subscribers`.
- Boot semantics: `Store::lane_tickets_all()` (unbounded, oldest first) feeds
  restore; queued rows replay in `(queuedMs, id suffix)` order, `running` rows
  become `Failed { exit: -1 }` with the message `lane runner was interrupted by
  a restart`, and terminal rows are only read, never re-published, so a boot
  cannot wipe their `finished`/`outputPath`. The next id is always max seen + 1
  and a persisted id is never reissued. An empty store is a valid fresh boot; a
  source failure aborts boot instead of silently starting with an empty queue.
- Coalescing key `(lane, normalized command, cwd, tree_rev)` requires
  `tree_rev: Some(non-empty)` and only matches a queued or running ticket: a
  match returns that id with `subscribed = true` and appends the caller to
  `Ticket.subscribers` (deduplicated). Terminal tickets are never reused, and
  a missing `tree_rev` always creates a distinct ticket.
- `tree_rev` is caller-supplied and opaque; the runtime's producer
  (`crates/oc-runtime/src/lane/mod.rs`) hashes `git rev-parse --short HEAD`
  with `git status --porcelain=v2 --untracked-files=no` under a 500 ms budget
  per probe and returns `None` on any failure, which disables coalescing but
  never fails a submission. Only `cargo:` lanes compute it.
- The production runner (`ProcessLaneRunner`) and the SSE event adapter live in
  `oc-runtime`/`oc-cli`. The store persists ticket history
  (`save_lane_ticket`/`lane_tickets`), and `lane_tickets_all` feeds restore;
  coalescing state is in memory only, so `tree_rev`/`subscribers` are never
  persisted and a restart drops coalescing without dropping a run.

### `oc-antigravity`
- `AntigravityService::{open, in_memory_for_test, start_oauth, poll_oauth,
  cancel_oauth, complete_oauth, list_accounts_status, select_account,
  set_account_enabled, remove_account, remove_by_credential_id,
  select_by_credential_id, usable_account_count, has_usable_accounts,
  overlay_catalog, antigravity_model_infos, refresh_quotas_and_models,
  spawn_background_version_check}`.
- Implements `oc_llm::LlmTransportHook` in `gateway.rs` for `google`
  Antigravity models and active Antigravity-backed OAuth credentials.
- Exposes `GoogleSearchTool` (`google_search`, `oc_tools::Tool`) and V4
  `antigravity-accounts.json` persistence with V1–V3 migration. See
  [doc 26](26-antigravity-auth.md).

### `oc-server` route modules
Every route group is a module under `crates/oc-server/src/data/` exposing

```rust
pub fn routes() -> axum::Router<crate::AppState>;
```

plus its handlers. `lib.rs` builds one protected router and `.merge()`s the
modules (`session_ext`, `shells`, `experimental`, `mcp`, `lsp`, `plugin`,
`lane`, `observer`, `integration`, `antigravity`, `quotas`, `worktree`), then
applies the auth middleware. Rules:

- a route is registered exactly once; a module never owns a stateful router;
- handlers return `Result<_, ErrorResponse>`/`Json<...>` and use the shared
  helpers (`require_store`, `require_config`, `no_content`, `schema_error`,
  `query_error`, `session_not_found`, `runtime_unavailable`);
- core routes whose handler needs no manager stay in `lib.rs` (sessions,
  messages, config, models, fs, vcs, project, lists);
- anything not implemented answers a truthful `501`/`503` with the documented
  error tag — never an empty success.

Landed groups: project/VCS, session CRUD + prompt/command/synthetic/compact,
messages/inbox/log, permissions (request + saved), forms, fork/revert/context/
diff/view/environment, export/import/stats/instructions/skill activation,
shell manager + session terminals + persistent PTY, fs + experimental
write/watch, MCP (list/resource/connect/disconnect/PUT/DELETE config),
LSP (status/diagnostics/reload), plugin (inventory/check/update/RPC),
experimental memory/goal/observer/subagent/lane/schedule, credentials +
integrations, worktrees, PTY, WebSocket upgrades.

`GET /api/fs/list` and `GET /api/fs/find` serve an additive superset of the
authoritative `FileSystem.Entry` (generated `FileSystemEntry`), which is only
`{path, type}`: this server returns `{name, path, absolute, type, size,
modified}`, compatible because JSON clients ignore unknown fields. `path` is
relative to the requested location root, `/`-separated, even when `path=`
targets a nested directory; a picker directory outside the location has no
relative form, so `path` falls back to the absolute text. `absolute` is always
serialized with the resolved path; `modified` is epoch ms. `type` is always
`file` or `directory`: a symlink is normalized from its resolved target, and an
entry whose link target is missing (or is not a file/directory) is skipped.

### `AppState` (server)
- One cloneable struct; every heavy field is `Option<Arc<…>>` and has a
  `with_*` builder plus accessor: `store`, `config`, `catalog`, `runtime`,
  `memory`, `observer`, `permissions`, `pty`, `mcp`, `lsp`, `plugins`,
  `lanes`, `tickets`, `antigravity` (`with_antigravity`/`antigravity`), `llm`
  (`with_llm`/`llm`, the shared `SharedLlmClient` whose HTTP pool and captured
  rate-limit headers feed `/api/quotas`). `quota_cache` is not optional: it is
  an always-present `Arc<RwLock<QuotaCache>>` with a crate-private accessor.
- `lanes` is the attached `oc_lanes::LaneService`; lane routes answer `503
  ServiceUnavailableError` when it is absent.
- `with_lsp` also bridges the supervisor's diagnostics stream onto the SSE bus.
- `bus()` is the `broadcast::Sender<Arc<EventEnvelope>>` (capacity 4096);
  `publish` is the only way routes emit events.

### MCP manager cache
- `data::mcp::McpManager` owns one `McpClient` per configured server and a
  `tool_cache: RwLock<Vec<(Arc<McpClient>, McpTool)>>`.
- `connected_tools()` is synchronous and returns the cached snapshot;
  connect/disconnect and `refresh_tool_cache()` rebuild it after the client
  set changes. `McpToolProvider` (in `data::mcp_tools`) reads that snapshot per
  execution, so connecting a server takes effect on the next turn without a
  restart.

### Observer model resolution (`oc-server`)
- `observer_host::build_for_state` resolves the judge through
  `SharedLlmClient::resolve_model`, so an attached `LlmTransportHook` (for
  example the Antigravity gateway) can own a model the startup catalog
  snapshot does not contain; it does not use `Catalog::resolve` directly.
- First non-empty candidate wins after trimming: `observer.model`,
  `observer.digest_model`, `subagents.digest_model`, the root `model`. No
  candidate, a failed resolution, or an `Unsupported` protocol disables the
  observer and logs one line; credentials are resolved per request, so a
  missing env var alone does not disable it.

### Schedule runner (`oc-cli`)
- `spawn_schedule_runner(AppState, CancellationToken)` starts after the runtime
  owns the store; it returns `None` when either is absent. It ticks every
  second (`SCHEDULE_TICK`), reads `due_schedules(now, 16)`
  (`SCHEDULE_BATCH`), and fires them sequentially.
- A firing computes the next due instant with `ScheduleWhen::next_after`,
  refuses archived/missing sessions, admits the prompt into the inbox as a
  `user` item with the stable id `msg_<scheduleID>_<runCount>` (duplicate ⇒
  conflict, so a crash cannot double-fire), records
  `mark_schedule_fired(outcome, next, run_count)`, publishes ephemeral
  `schedule.fired` `{scheduleID,sessionID,outcome}`, then `runtime.notify`.
- Outcome is `late` when ≥ 60 s behind (`SCHEDULE_LATE_AFTER_MS`), else
  `fired`; failures record `failed` so the same due instant is not retried on
  every tick.

### Provider auth (`oc-cli`)
- `opencode auth {login,list,status,quota,use,enable,disable,remove,models}`
  covers Google Antigravity OAuth (loopback or manual code) and plain provider
  API keys; the global flags are `-C/--directory`, `--db` and `--json`
  (structured output). `status`/`models` accept `--refresh`, `quota` accepts
  `--cached`.
- Interactive chat exposes the same pool through `/ag` (alias
  `/antigravity`): `status`, `quota [cached]`, `use|enable|disable <account>`,
  `models [refresh]`.

## Model control plane

Additive only: no existing payload, route or type was renamed or narrowed; every
change below is a new name. Wire sources stay authoritative (header of this
file).

### Event `session.model.fallback` (`oc-types`)

```rust
// crates/oc-types/src/payload/meta.rs
pub struct SessionModelFallback { session_id, from: ModelRef, to: ModelRef,
                                 kind: String, attempt: Option<u32> }
impl_event_payload!(SessionModelFallback, "session.model.fallback", true, 1);
```

Payload fields: `sessionID`, `from {providerID,id,variant?}`, `to
{providerID,id,variant?}`, `kind` (`empty_response|transport|rate_limit|quota`),
`attempt?` (1-based hop). `variant` and `attempt` are omitted when absent —
never `null`. `durable = true`, `version = 1`, and
`oc-store/src/event.rs:178` becomes a projection arm writing the durable model
for the event. It mirrors the existing `session.model.selected` payload; the
fallback record is the event log, not a new table.

### `LlmError::Failure` + `FailureKind` (`oc-llm`)

```rust
pub enum FailureKind { EmptyResponse, Transport, RateLimit, Quota, Auth, InvalidRequest, Unknown }
pub enum LlmError { /* existing variants unchanged, */ Failure { kind: FailureKind, message: String } }
```

Additive variant: every existing `LlmError` arm keeps its name and shape, so
matches that were exhaustive before only gain one arm. `is_retryable` truth
table:

| kind | `is_retryable()` |
|---|---|
| `EmptyResponse` | true |
| `Transport` | true |
| `RateLimit` | true |
| `Quota` | true |
| `Auth` | false |
| `InvalidRequest` | false |
| `ModelUnavailable` | false |
| `Unknown` | false |

Retryable = `EmptyResponse` | `Transport` | `RateLimit` | `Quota`; not
retryable = `Auth` | `InvalidRequest` | `ModelUnavailable` | `Unknown`.

Automatic turn model fallback is disabled: `FallbackPolicy::allows` always
returns `false` and turns never switch models mid-step when a provider call
fails after same-model key rotation. `models.fallback.on` and
`models.fallback.max_hops` remain accepted in configuration and wire payloads
for compatibility (`max_hops` defaults to `0`). `Http { status: 429 }` keeps its
transport retry (including `"rate limit exceeded"` responses, while billing/quota
messages remain non-retryable).

`oc_llm::classify_provider_message(&str) -> FailureKind` turns an opaque provider
message into one of these kinds (lowercase substring rules, first group wins:
model unavailable → quota → rate limit → auth → invalid request → `unknown`), and
the runtime raises `LlmError::Failure { kind, message }` instead of
`LlmError::Provider` for a known kind. `oc_llm::ModelHealth` remains shared
with `AppState::model_health()` for explicit control-plane reporting.

### Event `session.request.envelope` (`oc-types`, `oc-runtime`)

Log-only durable event publishing the exact model request of one step. It
exists because the request was **not** a pure function of the session log:
`crates/oc-runtime/src/runtime/state.rs:209-215` derives base messages from
`session_message` rows, but `crates/oc-runtime/src/turn/run.rs:275-353` then
appends six model-visible splices that were unlogged before this wave (memory
supplement, output-limit continuation, verify nudge, stall notice,
context-pressure notice, step-budget warning / final call). Each splice now has
a `RequestInjection` kind at the push site, so the uncomputable half of the
request is recorded instead of lost.

```rust
// crates/oc-types/src/payload/request.rs
pub struct SessionRequestEnvelope {
    pub session_id: SessionId,
    pub assistant_message_id: MessageId,
    pub step: u32,
    pub provider: String,
    pub model: String,
    pub tools: Vec<String>,          // exact order sent
    pub system_digest: String,       // digest of joined system text; text not duplicated
    pub base_messages: u32,          // count derived from session_message
    pub base_digest: String,         // digest over base message texts, in order
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub injections: Vec<RequestInjection>,
    pub created: i64,                // epoch ms
}

pub struct RequestInjection {
    pub kind: String,   // memory-supplement | output-continuation | verify-nudge
                        // | stall-notice | context-pressure | step-budget
                        // | max-steps | final-call
    pub text: String,   // the exact appended text
}
```

Event type `session.request.envelope`, `DURABLE = true`, `VERSION = 1`;
`injections` is omitted when empty, never `null`. It is **log-only**: it writes
no `session_message` row, never enters the transcript, and does not affect
`keepID`, token estimates or `last_user_text`. That is the whole point —
persisting the splices as message rows would (a) inflate
`estimate_turn_tokens` and cause premature compaction, (b) make each splice a
`keepID` candidate so its text is replayed every turn, (c) corrupt
`last_user_text` (`crates/oc-runtime/src/prompt/transcript.rs:256`) and hence
`stats.user_text` and memory recall, and (d) perturb the length-based gates
`context.len() >= 6` and `last_compacted_len`. Recording the request beside the
transcript instead of in it makes the request reconstructable by replay (the
splices, which are not recomputable) plus recomputation (the base messages,
which are).

**Status:** **landed and verified 2026-11-23.** The payload is
`crates/oc-types/src/payload/request.rs`
(`impl_event_payload!(SessionRequestEnvelope, "session.request.envelope", true, 1)`
at `:169`), re-exported from `payload/mod.rs:104` and `lib.rs:57`. The emitter is
`crates/oc-runtime/src/turn/request_envelope.rs` (the seed and the canonical
renderer) plus `crates/oc-runtime/src/turn/stream.rs:467-490`, which emits the
event after `session.step.started` and before the first byte is requested. Four
tests in `crates/oc-runtime/tests/runtime/request_envelope.rs`:
`request_envelope_is_durable_and_writes_no_message_row` (durable, zero
`session_message` rows), `request_envelope_reconstructs_the_request_from_the_log`
(an *independent* renderer recomputes `baseDigest` from the rows, and
`base + injections` equals the request the provider actually received),
`hook_probe_sees_the_real_request_and_owns_the_credential_pool`, and
`step_streamed_is_published_only_after_the_stamp_is_committed`. The block above
was re-checked against the landed struct: field names, `serde(rename_all =
"camelCase")`, `skip_serializing_if = "Vec::is_empty"` on `injections`,
`DURABLE = true` and `VERSION = 1` all match.

### `resolve_model_with` + `CandidateFilter` (`oc-subagents`)

```rust
pub struct CandidateFilter<'a> { pub chain: &'a [String], pub deny: &'a [String] }
pub fn resolve_model_with(.., filter: Option<&CandidateFilter<'_>>) -> Result<ModelChoice, ModelError>;
```

Signature-compatible: `resolve_model` keeps its arguments and delegates with
`None`. `chain` is a preference order inserted at position 2 (after the call
model, before the agent rows) and `deny` removes models variant-insensitively
(quota is per `provider/id`). Resolution never aborts on a bad candidate:
`ModelChoice { reference, source, skipped }` carries the winner plus every
`ModelSkip { reference, source, reason }` (`reason`:
`malformed | not_available | denied`). An empty list is
`ModelError::NoCandidate { chain }`; a non-empty list with nothing usable is
`ModelError::NoUsableCandidate { chain, skipped }` (replacing
`InvalidReference`/`Unavailable`), whose `Display` names every skip with reason
and source. `ModelSource` values are `Call | Chain | Agent | Default | Parent`
(`as_str()`: `call | chain | agent | default | parent`; `Fallback` was renamed to
`Chain`), so a chain pick is
distinguishable in the tool result (`docs/05`).

### `oc_store::usage::model_usage` (`oc-store`)

```rust
// crates/oc-store/src/usage.rs (new)
pub struct ModelUsageRow { provider_id, model_id, requests,
                           tokens_in, tokens_out, tokens_reasoning, cost: f64 }
pub async fn model_usage(..) -> Result<Vec<ModelUsageRow>, StoreError>;
```

Read-only rollup over `session_message.data` filtered by the `type` column; no
table, no migration, no write path. SQL and index plan are in
[docs/03](03-storage.md).

### Server modules

`GET /api/models/registry` lives in `crates/oc-server/src/data/models_registry.rs`
and `GET /api/experimental/usage/models` in
`crates/oc-server/src/data/usage_models.rs`; both expose `routes()` like every
other group (`docs/17` "oc-server route modules"). `PUT /api/config/models`
stays in `data/config_patch.rs` next to the existing allowlist, and `lib.rs`
route registration is the integration owner's edit only.



`Session.Metadata` is the only open object on the upstream wire
(`types.ts:65`, `openapi.json:22808-22809`; `Session.Info` itself is closed at
`openapi.json:21926-21927`). Locally defined keys live there and are documented
here so no crate invents a competing encoding:

- `subagentMode`, `maxDepth` (`oc-server/src/data/session/prompt.rs:76-77`),
  `environment` (`oc-server/src/data/session_ext/view.rs:37-51`), `lane`
  (spawn hint, `oc-runtime/src/subagent/spawn.rs:203`).
- `group` — an optional **non-empty string** label used to mark sessions that
  belong to the same logical wave across coordinators. Omitted when absent,
  never `null`; `""` is invalid and readers treat it as absent. It is
  descriptive only: no permission, policy, routing or lifecycle effect, not
  globally unique. There is **no** server-side `group` filter (the upstream
  `session.list` parameter list is fixed, `openapi.json:919-1068`;
  `SessionListInput` is `types.ts:3180-3190`), and no new wire field, column or
  table. See `docs/21-grouping.md`.

Because `PATCH /api/session/:sid` replaces the whole metadata object
(`oc-server/src/data/session/crud.rs:176-186`, `oc-store/src/session.rs:547-550`),
callers adding a local key must send the merged object.

## Ownership map (wave 1)

| Owner | Crate | Files it may create/edit |
|---|---|---|
| `Types` | `oc-types` | all files under `crates/oc-types/src`, its `Cargo.toml` |
| `Store` | `oc-store` | all files under `crates/oc-store/src`, its `Cargo.toml` |
| `Config` | `oc-config` | all files under `crates/oc-config/src`, its `Cargo.toml` |
| `Llm` | `oc-llm` | all files under `crates/oc-llm/src`, its `Cargo.toml` |
| `Tools` | `oc-tools` | all files under `crates/oc-tools/src`, its `Cargo.toml` |

No owner edits `Cargo.toml` at the workspace root, `oc-server`, `oc-cli`, or
another owner's crate. No owner runs `cargo check`/`test` until their own code
is complete, then runs exactly `cargo check -p <crate>` and
`cargo test -p <crate>` once. No `cargo fmt` on the workspace (the coordinator
runs it).

## Dependency rule

Wire entities are JSON. Store and Config own their row/document structs; they
must not wait for `oc-types` to grow. They may use `oc_types::{SessionId,
ProjectId, EventEnvelope}` and `serde_json::Value` for bodies. `oc-types` must
not depend on store/config/llm/tools.

## `oc-types`

Names that downstream code will import:

```rust
// id.rs (already exists)
pub struct EventId, SessionId, MessageId, ProjectId, PermissionRequestId, SavedPermissionId,
             ObservationId, GoalId, MemoryId, LaneId;

// session.rs
pub struct SessionInfo { /* exact wire Session.Info */ }
pub struct SessionTimes { pub created: i64, pub updated: i64, /* optional */ }
pub struct Tokens { pub input: i64, pub output: i64, pub reasoning: i64, pub cache: CacheTokens }
pub struct ModelRef { pub provider_id: String, pub id: String, pub variant: Option<String> }
pub struct LocationInfo { pub directory: String, pub project: ProjectRef, pub subpath: Option<String> }

// message.rs
pub struct Message { /* tagged by `type`, mirrors SessionMessage.Info */ }
pub enum MessageBody { User(UserMessage), Synthetic(SyntheticMessage), Assistant(AssistantMessage), ... }
pub struct AssistantMessage { pub agent: String, pub model: ModelRef, pub content: Vec<AssistantPart>, ... }
pub enum AssistantPart { Text(TextPart), Reasoning(ReasoningPart), Tool(ToolPart) }
pub struct ToolPart { pub id: String, pub name: String, pub executed: Option<bool>, pub state: ToolState, pub time: PartTimes }
pub enum ToolState { Streaming{input: String}, Running{input: Value, metadata: Option<Value>}, Completed{...}, Error{...} }

// inbox.rs
pub struct InboxItem { pub id: String, pub kind: InboxKind, pub payload: Value, pub delivery: Delivery }
pub enum InboxKind { User, Synthetic, Compaction, Move, Peer, Observer }
pub enum Delivery { Steer, Queue }

// event.rs (extend existing envelope)
pub struct Durable { aggregate_id: String, seq: i64, version: u32 }
// event payloads are `Value` for now; add typed structs only for events we emit in phase 1:
pub struct SessionCreated, SessionRenamed, SessionDeleted, SessionExecutionStarted,
           SessionExecutionSucceeded, SessionExecutionFailed, SessionExecutionInterrupted,
           SessionInboxEnqueued, SessionInboxDelivered, SessionTextDelta, SessionToolCalled, ...
```

Rules: optional fields omitted; `#[serde(rename)]` exactly as `types.ts`;
`type` is the discriminator; timestamps are `i64` ms. Tests must round-trip
fixtures copied from `types.ts` (hand-written JSON is fine).

## `oc-store`

```rust
pub struct Store { /* writer actor + read pool */ }
pub struct StoreError { /* thiserror */ }

pub struct NewSession {
    pub id: String, pub parent_id: Option<String>, pub project_id: String,
    pub directory: String, pub title: Option<String>, pub agent: Option<String>,
    pub model: Option<Value>, pub metadata: Option<Value>, pub permissions: Option<Value>,
    pub time_created: i64,
}
pub struct SessionQuery {
    pub limit: usize, pub order: Order, pub search: Option<String>,
    pub parent_id: Option<Option<String>>, pub archived: Option<bool>,
    pub directory: Option<String>, pub project: Option<String>, pub cursor: Option<String>,
}
pub struct SessionPage { pub items: Vec<Value>, pub cursor: Cursor }
pub struct Cursor { pub previous: Option<String>, pub next: Option<String> }

pub struct NewMessage {
    pub id: String, pub session_id: String, pub kind: String,
    pub data: Value, pub time_created: i64,
}
pub struct MessageQuery { pub session_id: String, pub limit: usize, pub order: Order,
    pub cursor: Option<String>, pub kind: Option<String> }
pub struct MessagePage { pub items: Vec<Value>, pub cursor: Cursor }

pub struct NewInbox { pub id: String, pub session_id: String, pub kind: String,
    pub payload: Value, pub delivery: Delivery }
pub struct NewEvent { pub id: String, pub aggregate_id: String, pub kind: String,
    pub data: Value, pub durable: bool, pub version: u32, pub created: i64 }

impl Store {
    pub async fn open(path: impl Into<std::path::PathBuf>) -> Result<Self, StoreError>;
    pub async fn open_memory() -> Result<Self, StoreError>;

    pub async fn create_session(&self, new: NewSession) -> Result<Value, StoreError>;
    pub async fn get_session(&self, id: &str) -> Result<Option<Value>, StoreError>;
    pub async fn list_sessions(&self, query: SessionQuery) -> Result<SessionPage, StoreError>;
    pub async fn patch_session(&self, id: &str, patch: SessionPatch) -> Result<(), StoreError>;
    pub async fn delete_session(&self, id: &str) -> Result<(), StoreError>;

    pub async fn append_message(&self, new: NewMessage) -> Result<Value, StoreError>;
    pub async fn get_message(&self, session_id: &str, id: &str) -> Result<Option<Value>, StoreError>;
    pub async fn list_messages(&self, query: MessageQuery) -> Result<MessagePage, StoreError>;
    pub async fn update_message(&self, session_id: &str, id: &str, data: Value) -> Result<(), StoreError>;

    pub async fn enqueue(&self, new: NewInbox) -> Result<Value, StoreError>;
    pub async fn list_inbox(&self, session_id: &str) -> Result<Vec<Value>, StoreError>;
    pub async fn promote_inbox(&self, session_id: &str, id: &str) -> Result<Option<Value>, StoreError>;
    pub async fn cancel_inbox(&self, session_id: &str, id: &str) -> Result<bool, StoreError>;
    pub async fn set_delivery(&self, session_id: &str, id: &str, delivery: Delivery) -> Result<(), StoreError>;

    pub async fn append_event(&self, event: NewEvent) -> Result<i64, StoreError>;
    pub async fn events_after(&self, session_id: &str, after: i64, limit: usize)
        -> Result<Vec<StoredEvent>, StoreError>;

    // Context boxes (`docs/25-context-boxes.md`), migration 0011.
    pub async fn write_box(&self, new: NewBox, append: bool) -> Result<Value, StoreError>;
    pub async fn get_box(&self, root_id: &str, key: &str, include_expired: bool)
        -> Result<Option<Value>, StoreError>;
    pub async fn list_boxes(&self, query: BoxQuery) -> Result<BoxPage, StoreError>;
    pub async fn delete_box(&self, root_id: &str, key: &str) -> Result<bool, StoreError>;
    pub async fn expire_temporary_boxes(&self, root_id: &str) -> Result<usize, StoreError>;
    pub async fn box_keys(&self, root_id: &str) -> Result<BTreeSet<String>, StoreError>;
}

pub struct NewBox {
    pub root_id: String, pub owner_session: String, pub key: String,
    pub title: Option<String>, pub description: Option<String>,
    pub kind: String,            // "stable" | "temporary"
    pub tags: Vec<String>, pub content: String,
}
pub struct BoxQuery {
    pub root_id: String, pub kind: Option<String>,
    pub include_expired: bool, pub limit: usize,
}
pub struct BoxPage { pub items: Vec<Value>, pub has_more: bool }
pub const BOX_KINDS: [&str; 2] = ["stable", "temporary"];
pub const MAX_BOX_CHARS: usize = 262_144;
pub const MAX_BOX_KEY_CHARS: usize = 64;
```

`context_box` is keyed `(root_id, key)` with a soft delete, so every session in
one tree shares the same boxes. Writes are create-or-update inside the writer
transaction (an append cannot lose a line), a soft-deleted row is recreated with
`revision = 1` and fresh timestamps, and an expired row is revived with
`time_expired` cleared and the revision bumped. `expire_temporary_boxes` stamps
only live `temporary` rows of the root and returns how many changed.

Implementation requirements:

- one writer task owns the write connection; reads use a small pool;
- WAL, `synchronous=NORMAL`, `busy_timeout=5000`, `foreign_keys=ON`;
- schema/migrations exactly per `docs/03-storage.md` (wire tables);
- `(session_id, seq)` unique; `append_event` allocates the seq in the same
  transaction as the projection; durable events persist by default;
- user/assistant message rows store `data` JSON exactly;
- tests: open temp DB, create/list/patch/delete session, message page
  pagination, inbox duplicate id ⇒ conflict, seq monotonic, reopen preserves
  data.

`Value` bodies are the wire object (e.g. `Session.Info`, `Message`). The server
passes them through; no second conversion.

## `oc-config`

```rust
pub struct ConfigService { /* immutable snapshot + source entries */ }
pub struct ConfigError { /* thiserror */ }
pub struct LoadOptions { pub directory: PathBuf, pub env: std::collections::HashMap<String,String> }

pub struct ConfigEntry { pub kind: EntryKind, pub path: Option<String>, pub info: Value }
pub enum EntryKind { Document, Directory }

impl ConfigService {
    pub fn load(options: LoadOptions) -> Result<Self, ConfigError>;
    pub fn entries(&self) -> &[ConfigEntry];             // GET /api/config
    pub fn merged(&self) -> &Value;
    pub fn agents(&self) -> Vec<Value>;                  // Agent.Info wire objects
    pub fn agent(&self, id: &str) -> Option<Value>;
    pub fn permissions(&self) -> &[PermissionRule];
    pub fn evaluate(&self, agent: Option<&str>, action: &str, resource: &str) -> Effect;
    pub fn skills_roots(&self) -> Vec<PathBuf>;
    pub fn mcp_servers(&self) -> Value;
    pub fn shell(&self) -> String;
    pub fn model(&self) -> Option<String>;
    // live `models` cell: PUT /api/config/models publishes here, the runtime
    // re-parses it per spawn/turn, and the registry reads the same cell.
    pub fn models_section(&self) -> Value;              // owned clone, `{}` when absent
    pub fn set_models(&self, models: Value);            // replaces + bumps revision
    pub fn models_revision(&self) -> u64;
    pub fn refresh_models_from_disk(&self) -> Result<(), String>;
}
/// Legacy agent ids -> canonical ids (build->engineer, plan->architect,
/// reviewer->review), case-insensitive and trimmed.
pub fn normalize_agent_id(id: &str) -> &str;
pub struct PermissionRule { pub action: String, pub resource: String, pub effect: Effect }
pub enum Effect { Allow, Deny, Ask }
```

Must implement: file discovery order, JSONC parse, `{env:}`/`{file:}`, V1
aliases, agent builtins (engineer/architect/review/general/explore/ui/docs plus the
hidden compaction/title/summary; legacy ids build/plan/reviewer normalize to
engineer/architect/review),
last-match-wins glob permissions, agent permission composition. Tests for each.
No network. `ConfigService::load` must not block on well-known/Console; those
are out of scope in phase 1.

## `oc-llm`

Define its own types; downstream imports:

```rust
pub struct ModelInfo { /* id, providerID, name, capabilities, limit, cost, variants, ... */ }
pub struct Catalog { /* bundled snapshot + config overlay */ }
impl Catalog {
    pub fn from_snapshot(json: &str) -> Result<Self, LlmError>;
    pub fn models(&self) -> Vec<ModelInfo>;
    pub fn providers(&self) -> Vec<ProviderInfo>;
    pub fn resolve(&self, reference: &str) -> Result<ResolvedModel, LlmError>;
}

pub struct LlmRequest { pub model: ResolvedModel, pub system: Vec<String>, pub messages: Vec<LlmMessage>,
    pub tools: Vec<ToolDef>, pub options: LlmOptions }
pub struct LlmClient { /* reqwest */ }
impl LlmClient {
    pub fn new() -> Result<Self, LlmError>;
    pub async fn stream(&self, request: LlmRequest) -> Result<LlmStream, LlmError>;
}
pub enum LlmEvent { StepStart, TextStart, TextDelta{delta:String}, TextEnd{text:String},
    ReasoningDelta{delta:String}, ToolInputStart{id:String,name:String},
    ToolInputDelta{id:String,delta:String}, ToolInputEnd{id:String,text:String},
    ToolCall{id:String,name:String,input:Value}, Finish{reason:String,usage:Usage},
    ProviderError{message:String} }
pub type LlmStream = futures_util::stream::BoxStream<'static, LlmEvent>;
```

Catalog: models.dev is a **dynamic source**, matching upstream, not a build
artifact. `catalog::refresh` fetches the published catalog
(`https://models.dev/api.json`, overridable with `OPENCODE_MODELS_URL`), caches
the raw body atomically, and **replaces** the served catalog: an additive merge
would keep a model upstream renamed, repriced or withdrew forever. The embedded
7.9 MB snapshot is only the cold-start floor for a machine that has never
fetched.

Replacement is gated on [`is_plausible`] (at least
`MIN_PLAUSIBLE_MODELS = 100` models), so a truncated response is a rejected
publish rather than an empty picker; a body that does not parse is also
rejected. A successful swap re-applies the same overlays the boot path uses
(config `providers`, Antigravity discovery) and then publishes
`models-dev.refreshed` so clients re-read the catalog.

**Provider-side `opencode-go` model discovery (issue #12).** Unlike the
Antigravity client, the `opencode-go` provider does serve a public OpenAI-style
model list: `GET https://opencode.ai/zen/go/v1/models` returned
`{"object":"list","data":[…]}` with 37 models on 2026-11-24, including
`hungerg-free` (canonical-free, context 1,000,000, $0/$0) and `hy4-flash-free`
(canonical `tencent/hy4-flash`, context 1,048,576, $0/$0). The endpoint is
unauthenticated; `/zen/go/v1/usage` is the authenticated quota sibling and is
unrelated. Public helpers in `catalog::refresh`: `OPENCODE_GO_PROVIDER_ID`,
`OPENCODE_GO_MODELS_URL`, `OPENCODE_GO_MODELS_URL_ENV`
(`OPENCODE_GO_MODELS_URL`, an env-overridable URL),
`OPENCODE_GO_CACHE_FILE`/`opencode_go_cache_path()`,
`MIN_PLAUSIBLE_PROVIDER_MODELS`, `opencode_go_models_url()`,
`parse_provider_models(body, provider_id) -> Result<Vec<ModelInfo>, LlmError>`,
`fetch_opencode_go_models_with_body`, `apply_provider_models`,
`provider_membership` and `is_plausible_provider_list`.

The feed rows are models.dev **v2** records, so `parse_provider_models` reshapes
the `data` array into a v2 provider document and runs it through
`Catalog::from_snapshot`. That is deliberate: an earlier hand-rolled field copy
kept only `id`/`name`/`canonical_id`/`limit`/`tools`, which degraded every
provider-discovered model to `Protocol::Unsupported` with an empty `cost` and no
variants. Going through the shared converter keeps the wire route
(`chat-completions` → OpenAI, `messages` → Anthropic, `generate-content` →
Gemini), the price band(s), modality collapse and reasoning variants. Rows whose
route this client cannot speak (`responses`, `systemone`, `images`,
`embeddings`) are returned with `Protocol::Unsupported` rather than dropped; the
transport rejects that protocol loudly and the registry keeps such rows out of
the chat picker. `canonical_id` is carried across explicitly because the v2
converter does not read it.

**The provider list is authoritative for membership.** `models.dev` still lists
models a Go subscription no longer includes, verified live on 2026-11-24:
`space-bunny`, `minimax-m3.1-flash-free`, `step-5-preview-free` and
`gemini-4-argon-contributor` answer `isn't included in OpenCode Go`, while
`glm-5.2`, `minimax-m2.7`, `mimo-v2.5`, `crouton-alpha-free` and
`longcat-2.5-free` answer `has been deprecated`. So the feed drives two
operations, in this order: `Catalog::upsert_provider_models` (the published
entries carry their own route and prices) and then
`Catalog::retain_provider_models` (ids the subscription dropped are withdrawn,
never offered). `apply_provider_models` is the single implementation of that
pairing, shared by the boot path, the background loop and
`POST /api/model/refresh`.

Membership is gated **relatively**, not against the flat
`MIN_PLAUSIBLE_PROVIDER_MODELS` (1): `is_plausible_provider_list(fetched,
served)` requires `fetched * 2 >= served`, so a truncated answer still upserts
what it published but withdraws nothing. Losing more than half a subscription
on one fetch is a bad response, not a decision.

There is no automatic model fallback and no hidden filtering here: a newly
published model appears because the feed lists it, and disappears only when the
feed stops listing it. `models.hidden` still governs picker visibility per
`AGENTS.md`.

A background loop refreshes the published catalog 30 s after boot (startup stays
offline, per `docs/15-performance.md`) and every 6 h, and a **second** loop on
the same cadence refreshes the `opencode-go` provider list
(`spawn_opencode_go_refresh_loop`), caching the raw body in
`opencode-go-models-cache.json`. Both loops re-apply the config `providers`
overlay and Antigravity discovery before swapping the cell, then publish
`models-dev.refreshed`. At startup the cached bodies are applied under the same
gates, so an offline restart serves the last fetched membership *and* stops
offering models withdrawn since the previous run. `refresh_now` is the one-shot
form: it fetches an **explicit** URL (never mutating process environment),
atomically caches the raw body and only then invokes `apply`; a failed fetch
never calls `apply`.

`POST /api/model/refresh` (oc-server, additive local route) invokes that
one-shot path with `refresh::models_url()` and `refresh::cache_path()`. Its
`apply` closure uses the same layering as the CLI boot refresh: `parse_live`,
the `is_plausible` gate, config `providers` overlay, Antigravity discovery
overlay, `CatalogCell::replace`, then publish `models-dev.refreshed`. It answers
`{"data":{"modelCount":N,"refreshed":true}}` only when the swap really happened;
a missing catalog cell is `503 ServiceUnavailableError`, while fetch, parse,
plausibility and overlay failures are `502 ProviderError` and neither swap the
cell nor publish the event. `GET /api/model` keeps its `{location,data:[...]}`
shape.

Event `models-dev.refreshed` carries `{data:{}}` with no extra fields; clients
use it as a cache-invalidation signal and re-read the catalog. It shares the
wire-legal empty-data shape with the other lifecycle events (generated
`types.ts` / `openapi.json`).

Variant dedup contract for `session.model.selected` and
`session.model.fallback`: when the next selection keeps the same `providerID`
and model `id` and its normalized variant is unchanged, the redundant durable
event is suppressed. Normalization treats absent, `""` and `"default"`
(case-insensitive) as the same variant; `"high"` → `"max"` is a real switch and
still patches the session and emits the event. Manual `POST /api/session/:sid/model`
with an unchanged selection likewise omits the notice event while remaining a
successful request. Catalog validation and resolution
still run before any dedup decision.

```rust
pub struct CatalogCell { /* RwLock<Arc<Catalog>> + revision */ }
impl CatalogCell {
    pub fn new(catalog: Catalog) -> Self;
    pub fn from_arc(catalog: Arc<Catalog>) -> Self;
    pub fn current(&self) -> Arc<Catalog>;      // pins one generation per turn
    pub fn replace(&self, catalog: Catalog);
    pub fn update(&self, apply: impl FnOnce(&mut Catalog));
    pub fn revision(&self) -> u64;
    pub fn provider(&self, id: &str) -> Option<ProviderInfo>;
    pub fn model(&self, provider_id: &str, id: &str) -> Option<ModelInfo>;
    pub fn models(&self) -> Vec<ModelInfo>;
    pub fn providers(&self) -> Vec<ProviderInfo>;
}
pub mod catalog::refresh {
    pub fn client() -> reqwest::Client;          // on-demand refreshes without a reqwest dep
    pub async fn fetch_live(client: &reqwest::Client, url: &str) -> Result<String, LlmError>;
    pub async fn refresh_now<F>(client: &reqwest::Client, url: &str,
        cache: &std::path::Path, apply: F) -> Result<(), String>
        where F: Fn(&str) -> Result<(), String>;
    pub fn parse_live(body: &str) -> Result<Catalog, LlmError>;
    pub fn is_plausible(catalog: &Catalog) -> bool;
    pub fn replace_live(cell: &CatalogCell, body: &str) -> Result<usize, LlmError>;
    pub fn spawn_refresh_loop<F>(apply: F) -> tokio::task::JoinHandle<()>
        where F: Fn(&str) -> Result<(), String> + Send + 'static;
    pub fn models_url() -> String;
    pub fn cache_path() -> std::path::PathBuf;
    pub fn read_cache(path: &std::path::Path) -> Option<Result<String, String>>;
    pub fn write_cache(path: &std::path::Path, body: &str) -> Result<(), String>;
}
```

`AppState::catalog()` now returns `Option<Arc<Catalog>>` (one pinned generation);
`AppState::with_catalog` still accepts a fixed `Arc<Catalog>` for tests and
`AppState::with_catalog_cell` takes the shared cell a live refresh swaps.
`RuntimeConfig::catalog` is an `Arc<CatalogCell>`, so the picker and the turn
loop cannot disagree about which models exist. The cache path is
`OPENCODE_MODELS_CACHE`, else `<data dir>/models-cache.json`.

OpenAI-compatible chat, Anthropic messages and Gemini streams, SSE parser,
retry/timeout policy. Tests use a local axum fake server; no real API keys, and
the live fetch is never exercised — replacement, the plausibility gate and the
cache are covered by fixtures.



## `oc-tools`

```rust
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;          // borrowed: MCP names are Strings
    fn description(&self) -> &str;
    fn schema(&self) -> Value;
    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError>;
}
pub struct ToolContext {
    pub session_id: String,
    pub directory: PathBuf,
    pub cwd: PathBuf,
    /// Cancelled when the owning execution is interrupted; detached helpers
    /// (the foreground subagent wait) stop through it instead of being orphaned.
    pub cancel: Option<CancellationToken>,
}
#[derive(Default)]
pub struct ToolResult { pub output: Option<String>, pub content: Option<Value>, pub metadata: Option<Value> }
pub struct ToolError { pub message: String, pub metadata: Option<Value> }  // thiserror + helpers
```

### `key_health` (read-only)

`crates/oc-tools/src/key_health.rs`, registered in `ToolRegistry::with_defaults`
and merged into every execution by the runtime (`turn/session.rs`). The tool owns
no state: `oc_tools::key_health::with_key_health_source` binds a
`KeyHealthSource` in task-local storage for one call, the same pattern
`code_map` uses for its semantic oracle (`turn/tools/run.rs`). The runtime's
implementation (`oc-runtime/src/key_health.rs`) joins
`Store::credential_pool` with `oc_llm::KeyPool::entries`.

- Arguments: none required; optional `provider` (string, exact provider id).
- Result: `{"summary": "<one line>", "providers": [{"providerID": "...",
  "keys": [...]}]}`. Each key carries `credentialID?`, `label?`, `active`,
  `limited`, and — only when `limited` — `cooldownUntilMs`, `kind`, `failures`.
  The environment key has `source: "env"` and no `credentialID`. Every optional
  field is omitted, never `null`. Key material is never present: the structs
  have no field for it.
- A provider with at least one key is always listed, even when nothing is
  cooling, so the model can say "2 keys for opencode-go, both healthy". An
  unknown `provider` filter yields an empty `providers` array, not an error.
- Without a bound source the tool returns a typed `ToolError` (no fake success).

Landed built-ins: `code_map`, `read`, `write`, `edit`, `symbol_edit`, `patch`,
`glob`, `grep`, `shell`, `todowrite`, `webfetch`, `websearch`, `key_health`. Exact
schemas/limits/error strings per `docs/12-tools.md`; tests cover edit
three-tier matching, CRLF, atomic symbol editing, patch grammar, glob/grep
limits, shell timeout/kill, webfetch SSRF guards. No `expectation` injection
here; the runtime adds it. Hosts may attach a semantic oracle
(`oc_tools::code_map::SemanticPrecisionOracle`) and an
`oc_runtime::EditDiagnosticsHook` (implemented over `LspSupervisor` in
`oc-server::data::lsp_tools`) for post-edit radar and `lsp`/`hybrid` precision
resolution.

Note the `&str` return type is deliberate: dynamic tool names (MCP) need to be
backed by an owned `String`. Do not change it back to `&'static str`.

## Module layout (oversized-file split)

The public paths above are unchanged. Former single-file modules are now
facades (`foo/mod.rs`, or `foo.rs` beside `foo/`) that re-export the same names
explicitly — no `pub use x::*`. Shared types stay in the facade; helpers called
across sibling modules are `pub(super)`.

| Crate | Facade → submodules |
|---|---|
| `oc-types` | `payload` → `lifecycle, compaction, meta, message, trace, step` |
| `oc-runtime` | `turn` → `types, assistant, budget, stats, session, stream, finalize, tools, permissions, compaction, pressure, repetition, stall, verify, run, settle`; `turn/tools` → `run, outcome, spill`; `subagent` → `settings, state, tree, protocol, wait, spawn, talk, roster, write, digest`; `subagent/spawn` → `continue_child, partition, prepare`; `observer` → `advice, repair, host, tick, repair_worker, strips`; `lane` → `runner, output, shell_tool, tool`; `prompt` → `sources, transcript, model, tests`; `skill` → `activate, write, lint, util, tests`; `runtime` → `traits, actor, interrupt, state` |
| `oc-server` | `data/session_ext` → `common, fork, revert, context, diff, view, forms, saved_permissions, export, import, stats, skill, instructions`; `data/session` → `bodies, crud, select, run, prompt, wait, events`; `data/integration` → `bodies, command, connect, credential, errors, registry, view`; `permission` → `state, rules, tests`; `observer_host` → `model, notifier, tests`; `data/shells` → `util, selection, http, persistent, manager/{output,process}`; `data/worktree` → `git, paths, plan, http`; `data/vcs` → `git, refs, status, diff, http`; shared `data/git.rs`; `data/antigravity.rs`, `data/quotas.rs` |
| `oc-llm` | `catalog` → `types, resolved, snapshot, overlay, resolve, reference, cost, source, convert, v1, variants, util, time, tests`; `quota` → `headers, time, types, util, probes/{mod,openrouter,deepseek,minimax,moonshot,opencode_go,siliconflow,zai}` |
| `oc-config` | `normalize` → `ctx, keys, root, copy, permissions, policies, model, agents, commands, resources, skills, compaction, mcp, providers, experimental`; `service` → `agents, commands, ctor, misc, skills`; `jsonc.rs`, `merge.rs` |
| `oc-store` | `schedule` → `when, ends, model, row, repo, zone, cron` |
| `oc-plugin` | `sidecar` → `config, status, process, inner, dispatch, register, error` |
| `oc-mcp` | `client` → `types, handler, session, request, catalog, accessors, handshake` |
| `oc-lsp` | `client` → `types, spawn, accessors, handshake, request, sync, diagnostics, lifecycle, dispatch, io, json` |
| `oc-observer` | `registry` → `strip, caps, store` |
| `oc-cli` | `cli, serve, bootstrap, schedule, lane, paths, auth, net, scribe, service, antigravity_cmd, run`; `antigravity_cmd` → `auth, pool, quota, format, chat`; `run` → `host, stream, permission, status` |
| `oc-antigravity` | `accounts, auth, config, constants, error, fingerprint, gateway, oauth, oauth_server, project, quota, rotation, schema, search, service, signature_cache, storage, stream, thinking, token, version`; `models` → `builtin, discovery, overlay`; `service` → `accounts, oauth, quotas, rotation, sync, token, types`; `gateway` → `pool, single, stream, validation`; `transform` → `request, gemini, claude, cross_model, model_resolver` |
| `oc-tools` | `code_map` → `files, graph_ops, map, native_index, outline, parsers, precision, radar, ripwire, ripwire_mcp`; `code_map/native_index` → `index, parse, ops_map, ops_symbol, ops_graph, ops_affected, ops_deep, pack, quality`; `code_map/graph_ops` → `mcp, exec_query, exec_deep, exec_batch`; `code_map/ripwire_mcp` → `pool, daemon, wire`; `symbol_edit` → `file, resolve, native, ripwire` |

Rules for future splits: move item bodies byte-identically; keep serde shapes,
route tables, error tags and publish ordering untouched; keep `#[allow]`
attributes attached; run the crate gate after each split.

## Handoff format (every agent)

Return ≤ 80 lines:

1. files created/changed;
2. public API deviations from this doc;
3. test command + result;
4. known gaps (only real ones, no TODOs in code);
5. anything the coordinator must wire.
