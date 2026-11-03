# 05 — Subagents, siblings and build lanes

Upstream v2 already has a `subagent` tool (renamed from `task`) and a session
tree. It deliberately forbids nesting (depth default 1), gives children no way
to talk to each other, and only lets a parent write into a **direct** child.
This document replaces those limits with explicit policy.

## Session tree model

Every session row carries:

```
id, parent_id?, root_id, depth, role ('primary'|'subagent'|'coordinator'),
spawn_allowed, child_count, agent, model, status, title, location, project_id
```

- `root_id` and `depth` are written at creation; no ancestor walks in hot paths
  (upstream uses an ancestor walk for the depth check).
- `child_count` is incremented/decremented transactionally with session
  create/delete; deletion cascades to descendants (upstream behavior).
- `role` is a policy hint, not a security boundary by itself.

## Model selection

Resolution order (first hit wins):

1. `subagent` tool call `model` (`provider/id[#variant]`).
2. The target agent's `model` (config `agent.<id>.model`).
3. `subagents.agents.<agent>.model`.
4. `subagents.default_model`.
5. Parent session's current model.

Every resolved model must be chat-capable and available; unknown variants are
rejected before the child session is created. The child's model is visible in
`Session.Info.model` and in the subagent bar.

## Configuration

```jsonc
{
  "subagents": {
    "enabled": true,
    "default_agent": "general",
    "default_model": "anthropic/claude-sonnet-4-5",
    "max_depth": 3,                     // 1 reproduces upstream
    "max_children_per_session": 8,
    "allow_spawn": true,                // children may spawn
    "background_default": true,
    "digest_model": "anthropic/claude-haiku-4-5",
    "siblings": {
      "enabled": true,
      "wake": true,                     // a peer message wakes an idle sibling
      "allow_parent": true,
      "max_messages_per_min": 20
    },
    "agents": {
      "explore": { "allow_spawn": false, "max_children": 0 },
      "build-master": { "model": "…", "role": "coordinator" }
    }
  }
}
```

Legacy `experimental.subagent_depth` is honored when `subagents.max_depth` is
absent; if both exist, `subagents.max_depth` wins and a config warning is
emitted. Upstream `agent.general`/`agent.explore` deny the `subagent` action by
default. This build keeps `explore` read-only (deny) and allows `general` to
spawn children subject to `subagents.max_depth`, `subagents.allow_spawn` and the
per-agent `subagents.agents.<id>.allow_spawn` override (explore disables it).
`subagents.timeout_seconds` (default 600) bounds both the foreground wait and
the background watcher; a child that never starts, dies, or is deleted settles
as `state="failed"` instead of hanging the parent.

## Tool contract

`subagent` (accepts the upstream field set; new fields optional):

```jsonc
{
  "agent": "general",
  "description": "short label shown in the tree",
  "prompt": "task text",
  "model": "provider/model#variant",   // optional
  "sessionID": "ses_…",                // continue a direct child / descendant
  "background": true,
  "role": "worker",                    // worker | coordinator
  "spawn": { "allow": false, "max_depth": 1 },
  "lane": "build"                      // hint for the lane service
}
```

Output (tool result metadata; a background spawn or a foreground wait that was
detached returns immediately, a settled foreground child returns its result):

```jsonc
{ "sessionID": "ses_…", "status": "running" }     // child not settled yet
{ "sessionID": "ses_…", "status": "completed" }   // foreground, child settled
```

Model-visible envelope (`state` is the child's settled state):

```
<subagent sessionID="ses_…" state="completed">final text</subagent>
<subagent sessionID="ses_…" state="failed">final text</subagent>
```

The reported state comes from the child session's durable `idle_outcome`, read
through `pub(super) fn child_state(messages: &[Value], durable: Option<&str>) -> Option<&'static str>`
(`succeeded`→`completed`, `failed`→`failed`, `interrupted`→`cancelled`). The
durable row is authoritative; the idle message is consulted only for states the
row does not carry, and a failure from either source wins. `child_state` never
defaults to `completed`: no recorded outcome means the child is not known to
have settled, and callers must not read that as success. A child whose state is
`failed` or `cancelled` is returned as a tool **error**, so the parent publishes
`session.tool.failed` rather than `session.tool.success`; the error text is the
envelope and the metadata carries `sessionID`/`status`. A deleted child reports
`state="failed"` with metadata `status: "failed"`.

The parent sees **only** this text plus the tool metadata (`sessionID`,
`status`; background delivery uses `childID`, `agent`, `state`). Everything else
(tool calls, intermediate reasoning, costs) is retrieved on demand through
`digest`:

```jsonc
// digest tool
{ "session_id": "ses_…", "question": "Which tests failed and why?", "max_tokens": 300 }
→ LLM summary of the child transcript/tool results, cached by content hash
```

`digest` uses `subagents.digest_model` (or the observer's digest model) and is
rate-limited per parent. It never returns raw tool output.

## Sibling communication

New tool `talk`:

```jsonc
{ "to": "ses_…", "kind": "note|question|answer", "text": "…", "request_reply": false }
```

Authorization is centralized in `SessionPolicy::can_message(from, to)`:

| Case | Allowed |
|---|---|
| `to.parent_id == from.parent_id` (siblings) and `siblings.enabled` | yes |
| `to.id == from.parent_id` (child reports up) and `siblings.allow_parent` | yes |
| `to.parent_id == from.id` (down to own child) | yes |
| `from.role == coordinator` and `to` is in `from.coordinator_scope` | yes |
| anything else | **deny** `SiblingScopeError` |

`coordinator_scope` is the subtree rooted at the coordinator's parent (its
siblings and their descendants), fixed at spawn time and stored on the session.

Delivery: a `peer` inbox row with `sender_session_id`, `sender_agent`, `kind`.
The target gets a `<peer from="ses_…" agent="…" kind="…">text</peer>` block.
Peer messages are not user messages; they never inherit user permissions and
are marked `metadata.source="peer"` in the transcript. `siblings.wake=false`
delivers on the next turn instead of waking an idle sibling.

`roster` tool returns ids/status/titles only:

```jsonc
{ "siblings": [...], "children": [...], "parent": {...} }
```

No transcript access through `roster`.

## Spawning subagents from subagents

- At child creation: `depth = parent.depth + 1`; reject if
  `depth > subagents.max_depth`.
- `spawn_allowed` on the child = `parent.spawn_allowed && call.spawn.allow
  && agent allows subagent`. A tool call that violates it fails with
  `SpawnForbiddenError` before any session is created.
- `child_count` is checked against `max_children_per_session` (and per-agent
  override) under a transaction; concurrent spawns cannot exceed it.
- The depth/child limits are also enforced by the `subagent` tool wrapper, so a
  model cannot bypass them by calling the HTTP route (HTTP clients are trusted,
  tools are not).

## Writing into subagent sessions

`session_write` tool:

```jsonc
{ "session_id": "ses_…", "text": "…", "delivery": "steer|queue", "interrupt": false }
```

Allowed when `SessionPolicy::can_write(from, to)`:

- `to` is any descendant of `from` (not just a direct child);
- or `from.role == coordinator` and `to` is in scope;
- or `to == from` (self-note).

Denied for siblings, parents, and unrelated sessions. This is stricter than the
HTTP `POST /api/session/:id/prompt` route, which stays trusted (loopback +
password) for UI/CLI use.

## Big brother / little brother: lanes

The problem: N agents each run `cargo test` in the same target directory; cargo
serializes on its lock, agents block, turns stall, tokens burn.

### Mechanism (does not rely on the model cooperating)

1. **Command classifier** parses the shell command (`shell-words`, no shell
   evaluation) and maps `cargo build|test|clippy|nextest|run`, `rustc`,
   `npm|pnpm|bun|yarn test`, `go test`, `pytest`, `dotnet test`, `gradle`,
   `mvn` to a resource key: `cargo:<canonical workspace>`, `node:<workspace>`,
   etc. Workspace = nearest `Cargo.toml` with `[workspace]` or the package root.
2. **LaneService** owns named lanes with concurrency and queue policy. Defaults:
   `cargo:<ws>` = 1, `node:<ws>` = 1, `network` = 4, `gpu` = 1, `default` = 8.
3. The `shell` tool submits classified commands to a lane and returns a
   **ticket**. No thread blocks; the tool reports progress and streams output
   when the ticket runs. `lane.wait` is an await on a channel with a deadline.
4. **Coordinator subagent** (`build-master`, role `coordinator`, built-in
   agent) gets the `lane` tool:
   - `lane.queue {lane, command, cwd, priority, owner}`
   - `lane.order {ticket, priority}`
   - `lane.status {ticket|lane}`
   - `lane.cancel {ticket}`
   - `lane.notify {session_id, text}`
   It decides ordering (format → clippy → test, or affected crate first),
   batches (`cargo nextest --no-fail-fast`), and messages siblings with results.
5. The main agent configures the coordinator and siblings through normal agent
   config and `subagent` calls; `build-master` is a template, not a special
   code path.
6. Rust specifics encoded in the built-in `rust-build-test` skill:
   - one `cargo` per target dir; parallel work only via distinct
     `CARGO_TARGET_DIR` or worktrees;
   - `cargo nextest` preferred, fallback `cargo test --no-fail-fast`;
   - `cargo check` before `clippy` before `test`; share `CARGO_BUILD_JOBS`;
   - never run `cargo clean` without the lane's exclusive ticket.

Lane correctness is enforced in Rust (semaphore + ticket table), so even an
agent that bypasses the coordinator cannot run two cargo jobs in one target
dir. The coordinator is the optimization layer, not the safety layer.

### Events and UI

- `lane.queued|started|finished|cancelled` (our extension, ephemeral).
- Subagent bar shows agent, model, lane, ticket, status, elapsed/expected.
- Session tree shows coordinator → workers with sibling links.

## Failure modes

| Failure | Behavior |
|---|---|
| Child crashes mid-turn | Parent gets `<subagent state="failed">`; observer may `restart_subagent` |
| Parent dies | Children are interrupted (`reason:"shutdown"`), durable claims remain; on boot each repaired child is settled to the parent as failed exactly once (no automatic resume; observers may `restart_subagent`) |
| Sibling message to a deleted session | Tool error `SessionNotFoundError`; sender is notified |
| Depth/child limit hit | Tool error before session creation; no partial state |
| Model unavailable | Resolution fails before spawn; error lists the resolved candidate chain |
| Sibling storm | `max_messages_per_min` per sender; excess is queued with a warning |
