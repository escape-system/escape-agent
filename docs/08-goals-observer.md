# 08 — Goals and the observer loop

The observer is an LLM that supervises the whole session tree using **only
terse flight strips**. It never reads transcripts by default, never sees tool
arguments or outputs, and acts through the same command API as the UI.

## 1. Expectation contract

Every tool call must declare what it expects, in seconds, and when the observer
may intervene. The runtime injects this into every tool JSON schema before the
provider request:

```jsonc
"expectation": {
  "why": "spawn 20 subagents; need a simple pass/fail list",
  "expect_seconds": 90,
  "intervene_after_seconds": 150
}
```

Rules:

- `expectation` is `required` in every injected schema (internal tools and MCP
  tools alike). For MCP, the object is stripped before dispatch.
- `why` ≤ 160 chars, one sentence, action + expected outcome. No secrets.
- `1 ≤ expect_seconds ≤ 3600`.
- `intervene_after_seconds ≥ expect_seconds + 2`, capped at 3600. Violations are
  corrected and flagged (`synthetic_expectation=true`), never fatal.
- If a provider omits the field anyway, the runtime derives defaults per tool
  class (read/glob/grep 10 s, edit/write 20 s, shell 300 s, subagent 600 s) and
  marks the strip as derived.
- The same tool call may be retried by the observer; the retry keeps the
  original `why` and gets a new strip with `retry_of`.

Why a schema field rather than a side channel: standard function-calling
providers cannot emit out-of-band metadata reliably, and MCP schemas can be
patched mechanically. Stripping happens in one dispatcher.

## 2. Observation registry

`oc-observer` keeps a registry per root session (in memory, mirrored to
`observation` rows):

```rust
struct FlightStrip {
    id: ObsId,                 // obs_…
    root_id: SessionId,
    session_id: SessionId,     // who called
    agent: AgentId,
    tool_call_id: ToolCallId,
    tool: String,
    why: String,               // the only free text
    expect_s: u32,
    intervene_s: u32,
    started_ms: i64,
    settled_ms: Option<i64>,
    state: Running | Ok | Error | Cancelled | TimedOut | Repaired,
    outcome: Option<OneLine>,  // "ok", "exit 1", "3 files", "timeout"
    parent_call: Option<ToolCallId>,
}
```

Only strips; no arguments, no outputs, no assistant text. `outcome` is produced
by a formatter with a hard 60-char cap.

## 3. Observer loop

One `ObserverActor` per root session. Dormant while nothing is active.

Wake conditions:

- the earliest `started_ms + intervene_s` deadline (timer);
- a strip settles with `Error`/`Cancelled`, or three settle in a row;
- a session execution fails or is interrupted (a user stop is the exception —
  see *User stop is terminal* below);
- a goal is set/changed/completed;
- a manual request from the UI (`POST /api/experimental/observer/check`);
- a stall: no strip settled and no step event for `observer.stall_seconds`
  (default 120) while execution is busy.

The host adviser (see *Compaction advice* below) also runs once per tick over
the active sessions; that pass is deterministic and does not call the LLM, so
it never counts against the coalescing/rate limits above.

Coalescing: at most one LLM call per `min_interval_ms` (2000), at most
`max_calls_per_min` (12). If more events arrive, one call carries all of them.

Input (hard cap `max_input_tokens`, default 600):

```jsonc
{
  // `done` is 0 until the goal is accepted `done`, then it equals `total`.
  "goal": {"id":"gol_…","objective":"…","status":"active","criteria":["…"],"done":0,"total":3},
  "root": {"id":"ses_…","agent":"build","model":"…","step":7,"state":"busy"},
  "active": [
    {"sid":"ses_…","agent":"general","tool":"subagent","why":"20 workers; pass/fail list",
     "age_s":95,"expect_s":90,"intervene_s":150,"state":"running"}
  ],
  "recent": [
    {"sid":"ses_…","tool":"shell","outcome":"exit 1","took_s":12,"why":"run focused test"}
  ],
  "tree": [{"sid":"ses_…","parent":"ses_…","agent":"general","state":"busy","depth":2}]
}
```

No message text, no tool arguments, no file contents.

Output (strict JSON, one object, validated; unknown fields ignored, unknown
action → `none`):

```jsonc
{
  "action": "none | nudge | inspect | repair | goal_done | goal_blocked | goal_failed",
  "target": "ses_…",
  "reason": "≤ 20 words, why you intervened",
  "message": "nudge text (≤ 400 chars)",
  "repair": {
    "kind": "retry_tool | cancel_tool | restart_subagent | compact_context | switch_model | repost",
    "args": {}
  },
  "goal_evidence": ["tool_call_id", "message_id"]
}
```

## 4. Actions

| Action | Effect | Guard |
|---|---|---|
| `none` | nothing | — |
| `nudge` | inject `synthetic` (`metadata.observer=true`) into target | max 2 per target per 10 min |
| `inspect` | `digest` over target session or tool call (≤ 300 tokens) | max 6 per 10 min |
| `repair.retry_tool` | re-run a failed tool call | idempotent tools only, or explicit `allow_retry` in tool metadata |
| `repair.cancel_tool` | cancel a hung tool | never on `subagent` (use restart) |
| `repair.restart_subagent` | cancel child execution, respawn with same prompt + observer note | depth/child limits still apply |
| `repair.compact_context` | force compaction on target | once per target per 10 min |
| `repair.switch_model` | switch target to `observer.escalation_model` | only if configured |
| `repair.repost` | re-deliver last user prompt to a stalled session | only if no user input pending |
| `goal_done` | mark goal done | requires evidence refs that exist |
| `goal_blocked` / `goal_failed` | mark goal, stop the loop | one terminal verdict |

### 4a. Compaction advice (host-side, deterministic)

The LLM observer sees flight strips only: `ObserverInput` carries no usage and
no `context_window` (its `root` header is agent/model/step/state), so it cannot
judge context pressure. The adviser is therefore **not** the LLM observer: it
is a host-side pass, `Runtime::advise_context_pressure` in
`crates/oc-runtime/src/observer/advice.rs`, driven by the observer ticker over
the currently active sessions, and it never calls a model.

Rules (fixed constants `ADVICE_THRESHOLD_PERCENT = 70`,
`ADVICE_COOLDOWN_MS = 600_000`):

- Pressure is the runtime's own `context_pressure()` estimate, the same number
  the turn loop's threshold path uses.
- It advises when the estimate is at or above **70% of the ceiling**. It stays
  silent when the runtime already protects the session (`compaction.auto` is on
  **or** `compaction.ceiling_tokens` is set, so the threshold path already
  runs), when no ceiling can be computed for the session, when a compaction is
  pending (an inbox `compaction` row, or a `compaction` message with
  `status: "running"`), when less than **10 minutes** have passed since the
  last advice for that session, or when the estimate is below the threshold.
  The cooldown is stamped only after the advice row is durable, the session is
  woken, and the event is published.
- One advice enqueues a durable `observer` inbox row (`delivery: steer`) with
  `metadata {observer: true, advice: "compact", estimateTokens,
  ceilingTokens, triggerTokens}` and publishes the durable v1
  `observer.advised {sessionID, estimateTokens, ceilingTokens, triggerTokens}`.
- It never forces compaction: no `compaction` inbox row, no `RepairCommand`, no
  goal/expectation mutation, no repair-budget consumption. Acting on the advice
  (calling `compact_context`, or finishing and handing off) stays the agent's
  decision.
- The cooldown map is process-local, so a restart forgets the clock but never
  the message; the advisory row is durable.

Repair budget: `max_repairs_per_10min` per root (default 5), plus per-target 2.
On exhaustion the observer emits `observer.escalated` and stops acting
automatically; the UI shows a banner.

## 5. User stop is terminal

When a session's latest settled execution was interrupted with reason `user`
(`session_v2.idle_reason = 'user'`, written atomically with
`idle_outcome = 'interrupted'`; see `docs/03`), the observer must treat that
session, and every descendant under it, as **off-limits to automation**:

- it must not wake on the interrupt event itself;
- it must not `nudge` the session or any descendant;
- it must not `repost` the last user prompt;
- it must not `restart_subagent` for the session or any of its descendants.

The guard is durable: it is read from storage (`SessionNodeRow.idle_reason`,
carried on the agent-tree rows the observer already consumes), not from process
memory, so a server restart does not clear it. It lifts only when the session
starts a new execution — typically the user's next prompt; the new turn's
settle writes a fresh `idle_outcome`/`idle_reason` pair (or clears the reason
on success), and the observer may supervise normally again.

Two consequences worth stating explicitly:

- **Observer repairs never self-classify as user stops.** An observer-initiated
  interrupt of a child execution (for example the cancel half of
  `restart_subagent`) uses reason `superseded`, so the guard above does not
  latch on the observer's own action.
- **The goal is not mutated by a stop.** A user stop leaves
  `goal.status` unchanged (`active` stays `active`); the suppression is purely
  behavioral — the observer simply does not act on that subtree. If the user
  never prompts again, the goal waits rather than being marked blocked or
  failed.

Manual `POST /api/experimental/observer/check` is subject to the same rule: a
forced check may inspect and report on a user-stopped session (its strips,
outcome, goal state), but it must not re-raise it — no nudge, no `repost`, no
`restart_subagent` for that session or its descendants.

## 6. Repair notification

Every repair writes, in one transaction:

1. `observer_repair` row (kind, target, reason, args hash, attempt).
2. `observer.repaired` event `{rootID, target, kind, reason, attempt, budgetLeft}`.
3. A synthetic message into the affected session:
   `<observer repair="restart_subagent">…reason…</observer>`.
4. A one-line synthetic into the root session (unless target == root).

The UI renders repair rows on the timeline and in the observer panel. Nothing
is silent.

## 7. Goal system

A goal belongs to a root session. One active goal per root.

```sql
goal(id, root_id, objective, criteria, status, rounds, stalls,
     last_progress, evidence, model, time_created, time_updated)
```

Tools:

- `goal_set {objective, criteria[]}` — fails if an active goal exists.
- `goal_update {progress?, blocked_reason?}` — agent-reported progress.
- `goal_complete {evidence[]}` — asks the observer to judge; does not mark done
  directly.
- `goal_status` — returns status, rounds, stalls, evidence.

Statuses: `active | paused | blocked | done | failed | cancelled`.

The goal object served by `GET/POST/PATCH /api/experimental/goal` and its
`/history` route, and the `goal` object in a `goal.updated` event, both carry
`done` and `total` as integers. Neither is a column: the store derives
`total` from `criteria` and `done` as `total` only for a row whose status is
`done`, otherwise `0` (`docs/08`, this section). They are never `null`.

Prompt block (instruction source `oc/goal`, cache-stable per goal version):

```
<goal id="gol_…" status="active" round="7">
Objective: …
Criteria:
1. … [done]
2. … [open]
Evidence: msg_…, tc_…
Next: …
</goal>
```

Criteria carry only `[done]`/`[open]`; evidence ids are opaque session and
message handles (`msg_`, `tc_`) listed once on the `Evidence:` line and are
never attributed to an individual criterion. `done` equals `total` only once
the goal itself is accepted `done`, otherwise it is `0`, so a done marker means
the whole goal was accepted, not that a specific criterion was verified.
Per-criterion attribution would need a new column and is intentionally out of
scope.

Observer judging (defaults copied from upstream `GoalPlugin`): max 50 rounds,
max 3 stalls, wait limit 30 min. Verdicts `done | continue | blocked | paused |
wait`; only the observer may set `done`, and only with evidence that resolves to
real message/tool ids. A `wait` verdict schedules a re-check with backoff; a
`blocked` verdict posts a user-visible question.

The goal is supervised **by the same observer loop** that supervises tool
strips: one actor, one budget, one audit trail.

## 8. Configuration

```jsonc
{
  "observer": {
    "enabled": true,
    "model": "anthropic/claude-haiku-4-5",
    "digest_model": "anthropic/claude-haiku-4-5",
    "escalation_model": null,
    "tick_ms": 1000,
    "min_interval_ms": 2000,
    "max_calls_per_min": 12,
    "max_input_tokens": 600,
    "stall_seconds": 120,
    "max_repairs_per_10min": 5,
    "nudge_max_chars": 400
  },
  "goal": {
    "enabled": true,
    "max_rounds": 50,
    "max_stalls": 3,
    "wait_limit_minutes": 30
  }
}
```

The observer model defaults to `subagents.digest_model`, then the root model.
If the root provider is unreachable, observer calls fail open (`none`) and
publish `observer.unavailable` once per 5 min; they never block the turn.

The host-side compaction adviser has **no config keys**: the 70% threshold and
the 10-minute cooldown are fixed constants. Only `compaction.auto` (its advice
is suppressed while auto-compaction already runs) and
`compaction.ceiling_tokens`/`buffer` (they move the numbers it reports)
influence it.

## 9. Why this stays cheap

- No transcript in the prompt; strips are ~40 tokens each, cap 24 active.
- Coalesced tick; no call when everything is on schedule.
- One observer per root, not per session.
- `digest` is optional and cached; the observer can act on the strip alone.
- All repair actions are commands, so the observer model cannot loop by
  re-entering the agent loop.
