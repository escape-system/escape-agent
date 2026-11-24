# 28 — dsh alignment: reconstructability of the model request

Design note. What we studied in DeepSeek Harness (dsh), the one invariant we
adopted, the mechanisms it uses to enforce it, the gap this exposed in our turn
loop, and what we deliberately did not copy. Research date 2026-11-23.

Everything under `docs/subsystems/…` and `packages/…` below refers to the dsh
tree; everything with a `crates/` path refers to this repository. Where a claim
was not verified, it says so.

## 1. What dsh is

- **DeepSeek Harness (dsh)**, version `0.2.3-alpha.2` (`package.json:3`).
- Commit `4b399c39a6d1479cd84ca591b4d7c88e24d38a96`
  (`Merge pull request #8961 … worktree-release-dsh-0.2.3-alpha.2`).
- TypeScript, ESM, `pnpm@11.28.5` (`package.json:7`), Node `^24.22.3 || >=26.13.1`.
- A **Cordis** monorepo: "plugins contribute services, typed events, and
  reversible effects to a shared context. Every part of the product is a plugin,
  including the model adapter, the tool registry, the session log, and the agent
  loop itself" (`docs/architecture.md:7-9`).
- Size, measured rather than quoted: 55 top-level groups under `packages/`,
  397 `package.json` manifests at `packages/*/*/package.json`, plus 6 under
  `apps/*`. (A "61 packages" figure circulating in notes counts entries in
  `packages/`, i.e. 55 group directories plus 6 files; it is not a manifest
  count. Use the 397+6 figure.)
- Full working tree available at `/root/deepseek-harness-src`, so every quote
  below was read from source, not from a summary.

Docs are organised per subsystem: `docs/architecture.md` (the map),
`docs/subsystems/session.md` (the log and its event map),
`docs/subsystems/llm-streaming.md` (the call path and its waterfalls),
`docs/agent-lifecycle.md` (sequence diagram), `docs/subsystems/compaction.md`.
There is a `.zh.md` sibling for most of them; we read the English files.

## 2. The invariant we aligned to

Quoted verbatim. From `docs/architecture.md:138`, "Session log":

> **Model-visible means logged.** Every model request must be reconstructable
> from the log. New model-visible inputs require session events. Plugins
> changing existing message content register [pure message
> projections](subsystems/session.md#plugin-owned-message-projections); detached
> readers supply the same definitions explicitly. Tool changes are
> capability-independent; [Session tool
> history](../packages/core/session/README.md) supplies provider declarations.

From `docs/subsystems/llm-streaming.md:1255-1258`, the `llm/stream` waterfall's
own second premise (the pivotal clause is at `:1257`):

> A LOOP-built request carries the process-local `markAgentLoopRequest` identity
> and arrives deep-frozen (mutation throws): its content is a pure function of
> the session log (the reconstructability Agent Note), so listeners read it,
> never rewrite it. Hand-built calls do not carry that marker; callers own their
> request inputs and must keep them unchanged until the stream settles.

And the closure statement at `docs/subsystems/llm-streaming.md:809`:

> The loop builds each request from logged state. `EpochHeader` records call
> config, marks the fields supplied by adapter defaults, and records the
> authoritative returned tool order … Together, the header and derived history
> make the request reconstructable from the session log.

Two consequences the wording makes explicit, and which matter for us:

1. **Logged is not the same as transcribed.** The header is logged session
   state but is *not* a message. `docs/subsystems/session.md:182`: "The event is
   not a `SurfaceEventType`: it produces no LLM message." The transcript is a
   projection of *surface* events; the log is larger than the transcript.
2. **The invariant is about the request, not about the transcript.** A reader
   must be able to rebuild exactly what went to the provider — system text,
   tools, call config, and any injected inputs — from durable state alone.

## 3. How dsh enforces it (three mechanisms)

### (a) A log-only durable `request/header` event

`docs/subsystems/session.md:180-195` defines it. The payload is `EpochHeader`:

```ts
interface EpochHeader {
  /** The conversation's call configuration (provider, model, reasoning effort, and sampling scalars). */
  config: LlmCallConfig
  /** Effective config fields materialized from the exact adapter rather than proposed by a caller. */
  adapterDefaults?: LlmCallConfigAdapterDefaults
  /** Assembled tool schemas; absent for a tool-less request. */
  tools?: ToolSchema[]
  /** Retired request text; system prompts belong to system/message events.
   * @persistenceReserved
   */
  system?: never
}
```

Key properties, all from the same section:

- Full snapshots only, with a `reason`: `'initial'` or `'resume'` at a
  loop-instance boundary, `'change'` when the envelope changed, `'series'` when
  an unchanged envelope starts an explicitly declared message series or follows
  a surface replacement. Later turns, steps and retries **inherit** the latest
  snapshot; an unchanged request is not re-logged per step.
- `foldRequestHeader(events)` reconstructs the header by selecting the latest
  snapshot — reconstruction is a fold, not a re-derivation.
- Canonical emptiness: `tools: []` and `adapterDefaults: {}` must be **omitted**,
  and `system` is forbidden in a header. That is the same "omitted, never null"
  rule we enforce in `AGENTS.md`, applied to a durable event.
- The rendered system prompt is deliberately *not* in the header: it is derived
  history, the `system/message` node at surface 0 plus later in-history system
  nodes. So an ordinary prompt change appends a system node and leaves the
  header untouched.
- Route metadata is a **separate** logged event, `request/context`
  (`docs/subsystems/session.md:209`), appended beside `request/header` only when
  provider, model, capacity or `systemPromptUpdate` changes. It is not a
  message either.

The important design move: dsh put the request envelope in the **same durable
log** as the transcript, while keeping it out of the **surface** that produces
LLM messages. One log, two event classes. That is what makes "reconstructable"
a property of the storage layer instead of a convention in the loop.

### (b) The log rejects non-JSON-serializable payloads at the source

`packages/core/session/src/index.ts:275-285`:

```ts
const dataSnapshot = snapshotJsonValue(data)
if (dataSnapshot === undefined) throw new Error(`${subject} carries non-JSON-serializable data`)
```

`Session.append` snapshots the payload before sequence assignment, so a live
object graph cannot be committed and then mutate underneath the log. The
accepted shape is documented at `packages/core/session/src/types.ts:383-384`
("MUST be JSON-serializable: `Session.append` runtime-validates all event data
with `isJsonValue`"). Validation also runs on persistence reads and seeds
(`packages/core/session/src/validation.ts:82-89,358-365`): a stored log that
does not round-trip losslessly is rejected rather than silently normalised.

This is the mechanism that makes replay trustworthy. Anything that cannot
survive `JSON.parse(JSON.stringify(x))` cannot be a logged input, so no durable
event can carry a handle, a closure, or a live cursor whose meaning depends on
the process that wrote it.

### (c) The loop-built request is deep-frozen

`packages/core/agent-loop/src/agent.ts:717,721` freezes the header and each
message it is about to dispatch; `deepFreeze` comes from
`@deepseek-ai/dsh-util-values` and recurses through nested objects and arrays.
`packages/llm/llm/src/call-config.ts:83-95` then registers the exact request
object in a process-local `WeakSet` (`markAgentLoopRequest` /
`isAgentLoopRequest`), so a listener downstream can tell a loop-built
conversation request from a hand-built auxiliary call.

The point is *unrepresentability*, not discipline: a waterfall listener that
tries `options.messages.push(...)` throws a `TypeError` before the provider sees
anything. dsh can then say "its content is a pure function of the session log"
and mean it — nothing after the loop can perturb the request, and the loop
itself built it from the log.

Note the honest limit of mechanism (c): it protects the request **in flight**.
It does not make the request reconstructable; that is (a). (c) is what stops a
plugin from breaking (a) after the fact.

## 4. Our verified gap, and the fix we chose

### The gap

Our context assembly is correct as far as it goes.
`crates/oc-runtime/src/runtime/state.rs:209-215`:

```rust
pub(crate) async fn load_context(&self, session: &str) -> Result<Vec<oc_llm::LlmMessage>, RuntimeError> {
    let messages = self.list_all_messages(session).await?;
    Ok(crate::prompt::messages_from_store(&messages))
}
```

Base messages are a pure function of `session_message` rows. Then the turn loop
appends model-visible inputs that were **not** logged before this wave.
`crates/oc-runtime/src/turn/run.rs:275-353` (line numbers at the time of
writing; each splice now also pushes a `RequestInjection`):

| Lines | Splice | Source |
|---|---|---|
| 282-288 | memory supplement | `turn.memory_supplement`, a diff against the in-memory pinned set |
| 289-295 | output-limit continuation | `prompt::CONTINUE_AFTER_OUTPUT_LIMIT_PROMPT` |
| 296-302 | verify nudge | `pending_verify_nudge`, built by `verify::build_verification_nudge_with_radar` (`run.rs:454`), scans the live filesystem/radar |
| 303-309 | stall notice | `pending_stall_notice`, turn-local (`run.rs:400`) |
| 310-316 | context-pressure notice | `context_pressure_notice`, derived from the live token estimate |
| 317-353 | step budget: `MAX_STEPS_PROMPT`, `STEP_BUDGET_WARNING`, or the "Checkpoint:" reminder | `cap` / `budget::can_extend_with_policy`, turn-local |

Six splices that were previously unlogged; all six are now recorded as
`injections` on the envelope. `crates/oc-runtime/src/turn/mod.rs:3-5` still
documents the neighbouring behaviour: "deltas are never persisted." Before the
envelope the durable log recorded the *transcript* faithfully and the *request*
not at all — a killed server, a replayed log, or a bug report containing only the
database could not say what the model was actually sent.

Three of the six are **not recomputable** after the fact (recorded in the frozen
contract box `contract.request-envelope` from the design exploration):

- the memory supplement is a diff against an in-memory pinned set that no longer
  exists after a restart;
- the verify nudge scans the live filesystem, which has moved on;
- the stall notice is turn-local state that was never written down.

The other three are recomputable *in principle* (continuation text is a
constant, context pressure and step budget derive from recorded numbers), but
not without re-running the loop's arithmetic against a log that has since grown.

### Why a log-only event, not message rows

The obvious cheap fix — persist the splices as `session_message` rows — is
wrong, and four concrete failures were proven before we rejected it:

1. **Token estimates inflate → premature compaction.** `estimate_turn_tokens`
   (`crates/oc-runtime/src/turn/pressure.rs:56`) sums the transcript. Six extra
   user messages per step push the estimate over the trigger, so a session
   compacts long before it needs to.
2. **`keepID` candidacy → text replayed forever.** `compaction_keep_id`
   (`crates/oc-runtime/src/turn/compaction.rs:696`) picks the retained tail by
   message. A persisted splice becomes a keep candidate and its text is replayed
   on every subsequent turn, duplicating the same instruction indefinitely.
3. **`last_user_text` corruption → wrong memory recall.** `last_user_text`
   (`crates/oc-runtime/src/prompt/transcript.rs:256`) returns the last
   user-role message. If that becomes a step-budget reminder, then
   `stats.user_text` is wrong and the memory-recall query is built from
   "<system-reminder>…" instead of what the user asked.
4. **Length-based gates perturbed.** `context.len() >= 6` and
   `last_compacted_len` are raw message counts; six synthetic rows per step
   change when guards fire.

Each of those is a behaviour change in the turn loop caused by a *recording*
change. That is exactly the coupling dsh avoids by keeping the header off the
surface.

### What we adopted

A durable **log-only** event, `session.request.envelope`, recording the exact
request: provider, model, tool order, a digest of the system text, the base
message count plus a digest over the base texts, and the literal injected texts
with their kinds. It writes no `session_message` row, so it cannot affect the
transcript, `keepID`, token estimates or `last_user_text` **by construction** —
the same argument as (a) above.

Reconstruction is then two operations, and a test asserts they agree:

- **recompute** the base messages from `session_message` (`messages_from_store`),
  digest them, and compare against the recorded `base_messages`/`base_digest`;
- **replay** the recorded `injections` verbatim into the reconstructed request,
  because three of six are not recomputable.

The full wire shape and the invariants the implementation holds live in
[17 — Interfaces](17-interfaces.md#event-sessionrequestenvelope-oc-types-oc-runtime).
**Status: landed and verified 2026-11-23** — the payload is
`crates/oc-types/src/payload/request.rs` and the emitter is
`crates/oc-runtime/src/turn/request_envelope.rs` plus
`crates/oc-runtime/src/turn/stream.rs:467-490`. The replay claim above is
asserted, not aspirational: `request_envelope_reconstructs_the_request_from_the_log`
(`crates/oc-runtime/tests/runtime/request_envelope.rs:149`) recomputes
`baseDigest` with an independent renderer and asserts that
`base + injections` equals the request the provider actually received, and
`request_envelope_is_durable_and_writes_no_message_row` (`:85`) asserts the
event lands in the log with zero added `session_message` rows.

Design constraints we adopted with it, taken from dsh:

- **Digests, not duplicated text, for anything already durable.** The system
  prompt and base messages are digest-only, so the event does not double the log
  size for the common case.
- **Canonical emptiness.** `injections` uses `skip_serializing_if` and is
  absent, never `[]`-with-nulls and never `null`.
- **Emit after the request is materialised and after `step.started` commits,
  before the first byte is requested.** Recording a request that was never sent
  would be as bad as not recording one that was.

## 5. What we adopted, and what we declined

### Adopted

- **The reconstructability invariant itself**, stated as a rule with a test
  behind it rather than a good intention.
- **Log-only durable events that never enter the transcript.** We already had
  the concept (`session.compaction.*` are durable but not surface messages); dsh
  made it explicit that this is the correct home for *inputs to* the request.
- **"A durable event must be a projection of committed state."** dsh's header is
  appended after the turn/step it belongs to; nothing is announced before it is
  true. We applied the same rule to the observer repair path this wave: no
  `observer.repaired` until the host confirms (`Receipt::Applied`), and "no
  answer" is unconfirmed, never success.
- **Monotonic policy expressed by prohibiting a return value.** dsh's post-tool
  decision type has exactly two shapes, `accept` and `block`
  (`packages/core/tools/src/contracts.ts:386-389`); there is no third "allow"
  variant a listener could return to widen what the harness permits, so a guard
  can only ever tighten or pass through. Our `FallbackPolicy::allows`
  (`crates/oc-runtime/src/subagent/settings.rs:88-90`) is the same idea at a
  smaller scale: the method exists, but the only implementation returns `false`,
  so no configuration and no call site can silently enable automatic model
  fallback. In both cases the policy holds because the API shape makes the
  forbidden answer unrepresentable, not because a runtime check remembers to
  refuse it.

### Declined, with reasons

- **WebSocket mux + `@Remote` streams.** dsh routes remote live state through a
  Gateway-owned stream mux (`packages/api/remotes/src/index.ts:37`). We ship
  axum + SSE with a documented 4096-frame overflow close, and the UI already
  recovers from a dropped stream by replaying the durable log. The transport is
  not where our bugs are. What *is* worth studying later is dsh's reconnect
  baseline contract — which frames a client may assume it has lost — not its
  mux. Porting the mux would add a second live path with no invariant behind it.
- **The Cordis plugin runtime.** Cordis's reversible effects and service
  injection are elegant, but our plugin surface is already a native
  `PluginRegistry` plus a JSON-RPC sidecar (`docs/10-plugins.md`), with a
  documented bridge matrix. Replacing it means re-porting every hook to get a
  capability we do not currently need, and it would put a framework between us
  and the invariants above.
- **The `SurfaceEventType` / projection framework.** dsh's split between surface
  events (which produce LLM messages), projection units, and everything else is
  more general than our model: we project in `oc-store`'s `project()`
  (`crates/oc-store/src/event.rs:101`) and derive messages in
  `messages_from_store`. Adopting the framework would be a rewrite of the event
  log for a distinction we can express as one column-free rule: an event either
  writes a `session_message` row or it does not.
- **`SubagentCapabilities` split.** dsh separates capabilities by realm and
  composes agent presets. Our agent policy is a permission document plus the
  hidden built-ins in `docs/14`. We have no current case that needs a per-realm
  capability object; adding one before it is needed would be speculative
  generality, which `AGENTS.md` forbids.

## 6. Open questions for the next owner

Stated as open, because they are:

1. **Should `request.envelope` carry per-message ids for exact replay?** Today
   the base messages are a count plus one digest. That proves equality but does
   not let a reader rebuild the array *from the event alone* — it must also read
   `session_message`. If we ever need a self-contained request artifact (export,
   bug report, eval harness), the event grows a list of
   `(message_id, digest)` pairs. Decide when a consumer exists, not before.
2. **Three of six injections are not recomputable, so the event must be treated
   as authoritative, not as a hint.** Any future "recompute the request instead
   of storing it" optimisation is therefore wrong unless it also records its
   inputs. Worth a comment at the emit site so the next optimiser cannot miss it.
3. **Injection kinds will drift.** The kind list
   (`memory-supplement | output-continuation | verify-nudge | stall-notice |
   context-pressure | step-budget | max-steps | final-call`) duplicates string
   literals that live in the turn loop. A new splice added at `run.rs:275-353`
   without a matching kind is a silent hole in the invariant. The reconstruct
   test in the contract only covers the splices that exist when it is written.
   A guard at the splice site (one enum, one push helper) would close it; nobody
   has built it.
4. **Retention.** Envelopes are one row per step — larger than the rest of the
   event log on a long session. There is no retention sweep in this workspace
   for any event type (`docs/18`, "no retention/maintenance sweep"). When one is
   added, the envelope is a candidate for compaction to digests-only, provided
   (2) is respected.
5. **Does the invariant extend to compaction requests?** dsh logs the
   compaction envelope like any other. Our compaction path builds its own prompt
   (`crates/oc-runtime/src/turn/compaction.rs`) and is not covered by the
   envelope as specified. If a compaction is ever audited the way a turn is, it
   needs the same treatment.
6. **The runtime-side repair receipt is no longer missing.** Closed 2026-11-23,
   and kept here only to record why it mattered: `ChannelExecutor::apply`
   (`crates/oc-runtime/src/observer/repair.rs:157-176`) now sends the command,
   awaits a `oneshot` receipt, and errors when the channel is dropped, so it can
   no longer answer `applied: true` at enqueue time. Every `RepairCommand`
   variant carries `reply: Option<RepairReply>` (`:28-90`) and
   `repair_worker.rs:200-220` answers with `apply_repair_command`'s real verdict;
   `repair_worker.rs:226` documents that every skip path returns
   `Ok(applied: false)`. The engine still refuses to publish success without a
   receipt (`Receipt::{Applied,Refused,Unconfirmed}`), so both halves agree now.
   Tests: `crates/oc-runtime/tests/observer_repairs.rs` (`r1`–`r6`, 17 tests) and
   `crates/oc-observer/tests/repair_receipts.rs` (10 tests).

## References

- dsh: `docs/architecture.md` (§ Session log), `docs/subsystems/session.md`
  (§ The request header event), `docs/subsystems/llm-streaming.md`
  (§ the `llm/stream` waterfall), `packages/core/session/src/index.ts`,
  `packages/llm/llm/src/call-config.ts`,
  `packages/core/agent-loop/src/agent.ts`.
- us: `docs/17-interfaces.md` (§ Event `session.request.envelope`,
  § `oc-llm`, § `oc-observer`), `docs/18-status.md` (§ Hardening wave),
  `docs/29-step-budget.md`.
