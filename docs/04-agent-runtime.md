# 04 — Agent runtime

Grounded in upstream v2 (`packages/core/src/session/**`). The Rust runtime keeps
the observable semantics and drops the Effect fiber machinery.

## Session lifecycle

```
prompt → inbox row (durable) → wake actor → execution.started
       → [step] → execution.succeeded | failed | interrupted
```

- `session_inbox` types: `user`, `synthetic`, `compaction`, `move`, plus our
  `peer` (subagent-to-sibling) and `observer` (repair note). Delivery is
  `steer` (promote mid-turn) or `queue` (next turn).
- One execution per session at a time. New prompts during an execution are
  admitted, not rejected; `steer` promotes at step boundaries. Only `steer`
  prompts are written to the transcript at admission; a `queue` row stays
  inbox-only (invisible to the running turn's requests) until the runtime
  promotes it as the next execution.
- `POST /interrupt?resume=` stops the current execution; `resume=true` keeps
  the claim so the next process/actor continues.
- `POST /background` detaches the current execution from the request; the
  actor keeps running. In phase 1 the only detachable blocking work is a
  foreground `subagent` wait: the flag makes the wait return `status: running`,
  the child keeps running and its final text arrives later as a synthetic
  `<subagent>` message. Shell `background: true` remains unsupported (see
  `docs/12`).

## One turn

1. **Promote inbox** (`SessionInbox.nextPromotable`): handle `move`,
   `compaction`, then user/synthetic/peer.
2. **Context**: session, agent, merged permissions, tool snapshot, instruction
   sources. Emit `session.instructions.updated` deltas when sources changed.
3. **Title**: generate if empty (hidden `title` agent, cheap model).
4. **Estimate prompt**: anchor-based. Find last assistant message with measured
   `input + cache.read + cache.write` for the same provider; estimate only
   messages after it at ~4 chars/token, image 1500, pdf 2000.
5. **Compaction**: ceiling = `context_window − max(15%, 16k)`. Auto when over;
   precompute at 85% when measured ≥ 64k. Summary is itself a session request
   with the fixed 7-section template; native compaction is optional.
6. **Request**: build transcript, `session.context` transform hook, deep-copy
   tool defs, apply modality bounds (images: 25 MiB trigger / 15 MiB target,
   oldest demoted to text), inject `expectation` into every tool schema.
7. **Stream**: provider chunks → protocol events. Persist text/reasoning/tool
   input on end-of-fragment, not per delta. Emit deltas on the bus at ~100 ms
   cadence.
8. **Tools**: for each `tool-call`, spawn a task. Ordering is not required for
   execution, but events are monotonic per `tool_call_id`. Results are
   truncated then committed, then `session.tool.success|failed`.
9. **Step end**: usage, cost, snapshot/files diff, finish reason.
10. **Continue** if any non-provider-executed tool ran; otherwise settle.
11. **Idle hook**: last chance to keep the turn alive (goal/observer may inject
    an inbox item). Never runs before durable events are committed.

## Prompt assembly (cache order matters)

System is two parts, exactly:

```
[ agent.system ?? default_system_prompt(tool_names),
  instructions_initial ]
```

`instructions_initial` is a hash-frozen concatenation ordered most-shared →
least-shared:

```
code-mode catalog → MCP catalog → references → skills index →
discovery (AGENTS.md/CLAUDE.md) → builtins (date, <env>) →
memory block (ours) → goal block (ours) → per-session entries
```

- Every source is keyed and hashed. A change emits a delta, never a rewrite of
  history: `session.instructions.updated {delta: {key: hash|"removed"}, text?}`.
- `agent.system` **replaces** the default prompt; instructions are always
  appended. A subagent with no system gets the same default as its parent.
- Skills are advertised as `<available_skills>` filtered by permission,
  `description` present, `autoinvoke !== false`, sorted by id.
- Memory and goal blocks are instruction sources (`oc/memory`, `oc/goal`), so
  they participate in delta updates and prompt caching.
- Per-request splices (volatile) go through the `session.context` hook, not the
  prefix; anything that changes per step must not invalidate the cache.

## Tool loop rules

- Tool defs are preserved even when `toolChoice: "none"` (max steps) so the
  prompt cache stays valid.
- Max steps: `agent.steps` (legacy `maxSteps`); the final step runs with
  `MAX_STEPS_PROMPT` and `toolChoice: "none"`.
- Output limit: `finish === "length"` → synthetic `CONTINUE_AFTER_OUTPUT_LIMIT`;
  three consecutive → `output-limit` failure.
- Mid-stream interruption → one `CONTINUE_AFTER_INCOMPLETE_STREAM` retry.
- Overflow before any output → one compaction recovery per step.
- Transport failure with `retry-full` and no output → one fresh-transport retry.
- Provider retries: exponential 2 s → 10 s, 10 attempts, jitter, honor
  `retry-after` ≤ 15 min; transport timeout capped at 3 retries.
- Timeouts: header 300 s, chunk 900 s (configurable per provider).

## Cancellation

Cancellation is cooperative but the finalizer is uninterruptible:

1. Set `stopping`, clear the wake doorbell, cancel the stream.
2. Emit `aborted` for running tools, settle the assistant message with
   `STEP_INTERRUPTED`.
3. Publish `session.execution.interrupted{reason: user|shutdown|superseded|inactivity}`.
4. Commit, then release the session claim (unless `resume`).

A naive `tokio::spawn` that drops the future loses 2–4 events. The Rust actor
owns the step future; cancellation is a token observed at await points, and the
finalizer runs in a separate `spawn` with a hard 5 s budget.

## Crash recovery

On boot, for every session with an execution claim:

- `running` assistant messages with no terminal event → mark
  `STEP_INTERRUPTED`, settle tool calls to `cancelled`.
- `running` compactions → failed, no retry.
- Inbox rows already `delivered` but not promoted → re-promote.
- Background subagent jobs → re-attach if the child still exists, else
  synthesize a failure to the parent.

All of this is one transaction per session, then one event per change.

## Extension seams (ours)

| Seam | Purpose |
|---|---|
| `session.context` transform | volatile splices (observer nudge, goal reminder) |
| pre-step gate | observer may veto/repair before a provider call |
| post-step | observer gets the terse step record (no text) |
| `session.idle` | goal/observer continuation decision |
| `tool.execute.before/after` | permission, lanes, expectation settlement |
| `instructions` source | memory, goal, skill index, digest |
