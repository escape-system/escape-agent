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
  `session.compaction.delta` is not implemented.

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
- Lane history: `save_lane_ticket(&Value)`, `lane_tickets(limit)`.
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
  models, providers, resolve}`; embedded upstream snapshot (7.9 MB).
- `LlmClient::stream(LlmRequest)` → `LlmStream`; `SharedLlmClient`;
  `LlmEvent`/`LlmMessage`/`ToolDef`/`LlmOptions`/`Usage` in `event.rs`;
  `Protocol::Unsupported` for providers not yet implemented.
- Retries 2s→10s, 10 attempts, `retry-after` ≤15 min; mid-stream failures emit
  `ProviderError` (no replay).

### `oc-tools`
- `Tool` trait (async, `name/description/schema/execute`), `ToolContext`,
  `ToolResult`, `ToolError`, `ToolRegistry::with_defaults` + sorted
  `definitions()`. `ToolContext` carries `session_id`, `directory`, `cwd` and
  an optional `cancel: Option<CancellationToken>`; the runtime cancels that
  token when the execution is interrupted, and tools that spawn detached
  helpers (the foreground subagent wait) use it to stop them instead of
  orphaning work when the tool future is dropped. Built-ins: `code_map`, `read`,
  `write`, `edit`, `patch`, `glob`, `grep`, `shell`, `todowrite`, `webfetch`;
  no permission/expectation logic (the runtime adds it).

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
    `box_delete`, `compact_context` (`docs/25-context-boxes.md`).
    `box_read`/`box_list` map to `box:read`, `box_write`/`box_delete` to
    `box:write` with the box key as the resource; `compact_context` maps to
    action `compaction`, resource `*`. `compact_context` never compacts inline:
    it commits an inbox row (`kind = "compaction"`, `delivery = Queue`,
    `reason = "manual"`), notifies the actor and returns `scheduled`.
- `CompactionConfig` (`crate::compaction`) is the runtime view of the merged
  `compaction` document: `{ auto, keep_tokens, buffer, ceiling_tokens }`.
  `auto` defaults to `false`, so token-threshold compaction runs only when the
  document opts in. `CompactionConfig::from_merged(&Value)` is the only
  constructor; `Runtime::compaction_config()` calls it.
- `tokens::ceiling(context_window, configured) -> u64` computes
  `context_window − max(15%, 16k)`; a configured `compaction.ceiling_tokens`
  may only lower it (`min`). The former hard-coded 150k cap is gone.
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
  siblings 20/min), `Policy::{spawn,can_message,can_write,resolve_model}`,
  `TreeLookup`/`SessionTree`, `SpawnBudget`, `SiblingBudget`, typed errors.
- Coordinator scope = parent + siblings + their descendants; siblings can talk
  only when enabled; writes are descendants/self/scope.

### `oc-lanes`
- `classify(command,cwd) -> Option<LaneRequest>` for cargo/rustc/node/go/
  pytest/dotnet/gradle/mvn; workspace-aware lane keys.
- `LaneService::{enqueue,status,ticket,cancel,reorder,wait,shutdown}` with
  priority FIFO per lane, semaphores, `LaneRunner`/`LaneEvents`/`Clock` traits;
  `tokens()` exposes the cancellation table for a runner.
- The production runner (`ProcessLaneRunner`) and the SSE event adapter live in
  `oc-runtime`/`oc-cli`. The store exposes ticket history
  (`save_lane_ticket`/`lane_tickets`) but the lane host does not persist
  tickets yet.

### `oc-server` route modules
Every route group is a module under `crates/oc-server/src/data/` exposing

```rust
pub fn routes() -> axum::Router<crate::AppState>;
```

plus its handlers. `lib.rs` builds one protected router and `.merge()`s the
modules (`session_ext`, `shells`, `experimental`, `mcp`, `lsp`, `plugin`,
`lane`, `observer`, `integration`, `worktree`), then applies the auth
middleware. Rules:

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
  `lanes`, `tickets`.
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

## Local conventions inside `Session.Metadata`

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
}
pub struct PermissionRule { pub action: String, pub resource: String, pub effect: Effect }
pub enum Effect { Allow, Deny, Ask }
```

Must implement: file discovery order, JSONC parse, `{env:}`/`{file:}`, V1
aliases, agent builtins (build/plan/general/explore/title/summary/compaction),
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

Catalog: the embedded models.dev snapshot (7.9 MB) is the source; `apply_config`
overlays the merged `providers` object. OpenAI-compatible chat, Anthropic
messages and Gemini streams, SSE parser, retry/timeout policy. Tests use a local
axum fake server; no real API keys.

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

Landed built-ins: `code_map`, `read`, `write`, `edit`, `patch`, `glob`, `grep`, `shell`,
`todowrite`, `webfetch`. Exact schemas/limits/error strings per
`docs/12-tools.md`; tests cover edit three-tier matching, CRLF, patch grammar,
glob/grep limits, shell timeout/kill, webfetch SSRF guards. No `expectation`
injection here; the runtime adds it.

Note the `&str` return type is deliberate: dynamic tool names (MCP) need to be
backed by an owned `String`. Do not change it back to `&'static str`.

## Module layout (oversized-file split)

The public paths above are unchanged. Former single-file modules are now
facades (`foo/mod.rs`, or `foo.rs` beside `foo/`) that re-export the same names
explicitly — no `pub use x::*`. Shared types stay in the facade; helpers called
across sibling modules are `pub(super)`.

| Crate | Facade → submodules |
|---|---|
| `oc-runtime` | `turn` → `types, assistant, stats, session, stream, finalize, tools, permissions, compaction`; `subagent` → `settings, state, tree, protocol, wait, spawn, talk, roster, write, digest`; `observer` → `repair, host, tick, repair_worker, strips`; `lane` → `runner, output, shell_tool, tool`; `prompt` → `sources, transcript, model, tests`; `skill` → `activate, write, lint, util, tests`; `runtime` → `traits, actor` |
| `oc-server` | `data/session_ext` → `common, fork, revert, context, diff, view, forms, saved_permissions, export, import, stats, skill, instructions`; `data/session` → `bodies, crud, select, run, prompt, wait, events`; `permission` → `state, rules, tests`; `observer_host` → `model, notifier, tests`; `data/shells` → `util, selection, http, persistent, manager/{output,process}`; `data/worktree` → `git, paths, plan, http`; `data/vcs` → `git, refs, status, diff, http`; shared `data/git.rs` |
| `oc-llm` | `catalog` → `types, resolved, snapshot, overlay, resolve, reference, cost, source, convert, v1, variants, util, time, tests` |
| `oc-config` | `normalize` → `ctx, keys, root, copy, permissions, policies, model, agents, commands, resources, skills, compaction, mcp, providers, experimental`; `jsonc.rs`, `merge.rs` |
| `oc-store` | `schedule` → `when, ends, model, row, repo, zone, cron` |
| `oc-plugin` | `sidecar` → `config, status, process, inner, dispatch, register, error` |
| `oc-mcp` | `client` → `types, handler, session, request, catalog, accessors, handshake` |
| `oc-lsp` | `client` → `types, spawn, accessors, handshake, request, sync, diagnostics, lifecycle, dispatch, io, json` |
| `oc-cli` | `cli, serve, bootstrap, schedule, lane, paths, auth, net` |

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
