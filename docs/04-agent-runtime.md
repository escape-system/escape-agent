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
- `POST /interrupt?resume=` stops the current execution. The `resume` query
  parameter is accepted for wire compatibility and **ignored**: there is no
  paused-execution state in the runtime, so nothing is kept claimed for a
  later continuation. The claim is always released once the turn settles; the
  next execution starts only from a new inbox row (typically the user's next
  prompt).
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
- Memory, box index and goal blocks are instruction sources (`oc/memory`,
  `oc/boxes`, `oc/goal`), so they participate in delta updates and prompt
  caching. `oc/boxes` renders keys/kinds/sizes only; content is loaded through
  `box_read`.
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

## Guards and finalization (Hermes Agent ports)

Three turn-level guards and one shell-level guard were ported from the Hermes
Agent (`verify_on_stop`, `tool_guardrails`, `repetition_guard`,
`terminal_tool_guards`). They are implemented in our crate; the paths below are
ours, not upstream's.

**Verify on stop** — `crates/oc-runtime/src/turn/verify.rs`, driven from
`turn/mod.rs` (`verify_on_stop_enabled`, `build_verification_nudge`).
Trigger: a turn wrote a code file (`write`/`edit` `path`, every path from a
`patch` envelope) with no *later* shell command matching the verification
list (`cargo test|check|clippy|build`, `npm/npx/yarn/pnpm test`, `pytest`,
`python -m unittest`, `go test`, `make test|check`, `ctest`, `mvn test`,
`gradle test`) in the same turn. Before the turn settles, the runtime appends a
user-role system nudge naming up to five changed files and asking for a real
verification command; the model gets at most **2** nudges per turn, then the
turn settles regardless. Docs-only edits do not trigger it (`.md`, `.txt`,
`.lock`, `README`, `LICENSE`, … are excluded by `is_code_file`).
Disable: default is **off**; enable with `verify_on_stop: true` (or
`agent.verify_on_stop`) in config, or `OPENCODE_VERIFY_ON_STOP=1|true|yes|on`,
which wins over config. Nothing new is published to the client — the nudge is a
synthetic message in the transcript.

**Stall guard** — `crates/oc-runtime/src/turn/stall.rs` (`StallGuard`), recorded
per tool call in `turn/mod.rs`. Trigger: the same tool name with byte-identical
canonical-JSON input **3 times consecutively**, or a cycle of period 2–4
repeated 3 times (A,B,A,B,A,B). History is capped at 32 calls and poller tools
(`process_manage`, `*_poll`) are exempt. Effect: the next request includes a
`[System: Stall Guard Warning: …]` user message telling the model to change
strategy; execution is not blocked and no failure is published. Always on; it
has no config or env switch.

**Repetition guard** — `crates/oc-runtime/src/turn/repetition.rs`
(`is_repetition_dominated`), consulted in `turn/mod.rs` before a
`finish == "length"` continuation. Trigger: ≥ 400 chars and either one ≥ 10-char
line repeated ≥ 4 times covering ≥ 50 % of the text, or a 60-char window
(sampled every 20 chars) seen ≥ 4 times and covering ≥ 50 % of the samples.
Effect: the synthetic `CONTINUE_AFTER_OUTPUT_LIMIT` retry is **suppressed**
(logged as `suppressed length continuation…`) and the turn settles with the
text it already has, instead of spending another step re-requesting a
continuation. Always on; no config.

**Shell hardline + foreground interceptor** —
`crates/oc-tools/src/shell_guard.rs` (`check_shell_command`), called by
`ShellTool::execute` before spawning anything (`crates/oc-tools/src/shell.rs`).
Two layers:

1. *Hardline blocklist* (irreversible): fork bombs; `shutdown -h/-r/now`,
   `reboot`, `poweroff`, `init 0`; redirects/`dd` to raw `/dev/sd*`,
   `/dev/nvme*`, `/dev/hd*`; recursive `rm` targeting `/`, `/*`, `~`, `$HOME`,
   `${HOME}` (note: `-r` alone is enough to block — the force flag is not
   required).
2. *Foreground long-lived interceptor*: a trailing `&` (but not `&&`),
   `nohup`/`disown`/`setsid` prefixes, known servers (`npm run dev`, `pnpm
   [run] dev`, `yarn dev`, `bun dev`, `vite`, `next dev`, `python[3] -m
   http.server`, `cargo watch`) and `docker compose up` without `-d`, because
   they would hold the foreground turn until its 120 s timeout.

`--help`/`--version`/`-h`/`-v` queries are exempt. A block is a normal
`ToolError` — the model sees the reason string, no permission ask is created,
and there is no config override; run bounded commands or the managed shell
instead.

## Cancellation

Cancellation is cooperative but the finalizer is uninterruptible:

1. Set `stopping`, clear the wake doorbell, cancel the stream.
2. Emit `aborted` for running tools, settle the assistant message with
   `STEP_INTERRUPTED`.
3. Publish `session.execution.interrupted{reason: user|shutdown|superseded|inactivity}`.
4. Commit, then release the session claim.

A **user interrupt** (`POST /interrupt` without `resume`, and the UI stop
button that calls it) is broader than one turn:

- It cancels the active execution of the session *and* of every live
  descendant (`Runtime::interrupt` walks the child tree, `interrupt_single` per
  session), so a stopped coordinator does not leave subagents running.
- It withdraws the session's and its descendants' undelivered `session_inbox`
  rows — queued prompts, synthetic/observer rows and queued `compaction` rows —
  before releasing the claim, so no post-stop turn can start from them.
- The settle step persists the reason durably (`session_v2.idle_reason`, see
  `docs/03`), in the same write as `idle_outcome = interrupted`.
- A user stop is **terminal for automation** until the user prompts again:
  goals, observer nudges/repairs and other automation must not restart the
  session on their own (`docs/08`).

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
