# 01 — Architecture

## Shape

One process, one binary, no Node/Bun on the hot path. Optional sidecars only
where the ecosystem forces it (MCP stdio servers, LSP servers, JS plugins).

```
                      ┌──────────────────────────────┐
  web UI (static) ────│  oc-server (axum/hyper)      │
  CLI / TUI      ────▶│  HTTP + SSE + WS             │
                      └──────────────┬───────────────┘
                                     │ commands / queries
                      ┌──────────────▼───────────────┐
                      │  oc-runtime                  │
                      │  SessionActor per session    │
                      │  turn loop, prompt, tools    │
                      └───┬────────┬────────┬────────┘
                          │        │        │
             ┌────────────▼─┐ ┌────▼────┐ ┌─▼─────────────┐
             │ oc-llm       │ │oc-tools │ │ oc-observer   │
             │ providers    │ │registry │ │ goals+repair  │
             └──────────────┘ └────┬────┘ └───────┬───────┘
                                   │              │
             ┌─────────────────────▼──────────────▼───────┐
             │ oc-store (SQLite WAL) + event bus           │
             └─────────────────────────────────────────────┘
```

Sidecars (each optional, each with its own lifecycle and RAM budget):

| Sidecar | When | Transport |
|---|---|---|
| MCP stdio servers | user config | stdin/stdout JSON-RPC |
| LSP servers | first file of a language | stdin/stdout JSON-RPC |
| JS plugin host (Bun) | any JS plugin configured | stdin/stdout JSON-RPC |
| `oc-pty` daemon | terminal feature used | named pipe, upstream frame protocol |

## Crates

| Crate | Responsibility | Must not depend on |
|---|---|---|
| `oc-types` | Wire types, IDs, events, errors, config types, JSON helpers | everything else |
| `oc-store` | SQLite schema, migrations, repositories, event log, inbox | runtime/server |
| `oc-config` | Discovery, merge/precedence, permissions, credentials | runtime/server |
| `oc-llm` | Provider registry, models.dev catalog, streaming, retries, token estimate | tools/store |
| `oc-runtime` | Session actors, turn loop, prompt assembly, compaction, instructions | server/UI |
| `oc-tools` | Tool registry, built-ins, lanes, subagent/session/memory/goal tools | server |
| `oc-observer` | Expectation registry, observer actor, goal judge, repair executor | server |
| `oc-memory` | Memory store, FTS retrieval, scribe | server |
| `oc-mcp` | MCP client, transports, OAuth, tool mapping | server |
| `oc-lsp` | LSP client manager, diagnostics cache, tools | server |
| `oc-plugin` | Native plugin host + JS sidecar bridge, transforms/hooks | server |
| `oc-pty` | Port of upstream `packages/process-daemon` (Rust already) | server |
| `oc-server` | axum routes, middleware, SSE/WS, static assets, OpenAPI | runtime internals |
| `oc-cli` | `opencode serve|run|auth|models|session|…` | — |
| `oc-testkit` | Golden fixtures, harness, fault injection | — |

`oc-server` talks to `oc-runtime` through a command enum and a query trait; no
handler reaches into a repository directly. This keeps the HTTP layer testable
without a live model.

## Concurrency model

- **One `SessionActor` per session**, owning that session's turn state. It is
  created lazily and parked after idle. Commands: `Prompt`, `Synthetic`,
  `Interrupt`, `SetAgent`, `SetModel`, `Compact`, `WritePeer`, `Repair`,
  `Shutdown`.
- **Inbox is durable.** A prompt is written to `session_inbox` and published
  before the actor is woken. Crash between write and wake is recoverable.
- **Tool calls run concurrently** in a `JoinSet`, each with a cancellation token
  and an expectation id. A tool result is committed exactly once (`tool_call`
  row state machine: `pending → running → ok|error|cancelled`).
- **Single SQLite writer.** A dedicated writer task owns the write connection
  and batches commits per turn step. Reads use an `r2d2` pool (WAL). No async
  task holds a connection across an `.await` that can block on a model.
- **Event bus** is a `tokio::sync::broadcast` of `Arc<EventFrame>`; each frame
  is serialized **once** to `Bytes` and fanned out. Per-subscriber bounded queue
  (4096); overflow closes that subscriber with the upstream error, never blocks
  the producer.
- **Observer** is a separate actor per root session, never inline in the turn
  loop. It can only act through the same command API as everything else.

## Turn data flow

1. `POST /api/session/:id/prompt` → validate → `oc-store` insert inbox row →
   publish `session.inbox.enqueued` → notify actor.
2. Actor promotes inbox → loads context → composes instructions → calls
   `oc-llm`.
3. LLM stream events are mapped to protocol events, persisted as they finish
   (not per delta), and published to the bus.
4. Tool calls get the `expectation` argument stripped, are authorized, then run
   concurrently; each start/settle updates the expectation registry and the
   observer.
5. Step ends → compaction check → continue if tools ran → settle execution
   (a step-budget exhaustion settles `succeeded`, then publishes
   `session.execution.step_limit`).
6. All durable writes are committed before the terminal event is published.

## Key decisions and why

| Decision | Why | Cost |
|---|---|---|
| Hand-written serde types, OpenAPI as oracle | Effect encoding semantics (`absent ≠ null`, branded IDs, flat event unions) do not map cleanly to codegen; generated types would be enormous and slow to deserialize | manual upkeep, guarded by golden fixtures |
| `axum` + `hyper` | boring, HTTP/1+2, first-class SSE/WS upgrade | hand-rolled SSE envelope |
| `rusqlite` (bundled) + writer actor | no system SQLite dependency on Windows, predictable latency, no async SQL compile times | manual async bridging |
| `reqwest` + hand-rolled SSE parser | control over provider headers, retries, timeouts, byte-level streaming | provider protocol work |
| JS plugins via sidecar | full compatibility with upstream Effect plugins is impossible in-process; sidecar is the only honest route | +~60–90 MB only when plugins exist |
| No embeddings by default | RAM and startup wins; FTS5 covers most recall | semantic recall optional |
| `mimalloc` | lower fragmentation/RSS on long sessions | one more native dep |

## Non-negotiable invariants

1. A terminal session event (`execution.succeeded|failed|interrupted`) is only
   published after its durable state is committed.
2. A tool call is never executed twice for the same `tool_call_id` unless the
   observer explicitly retries it, and that retry is recorded.
3. Every LLM request can be cancelled; cancellation always runs the
   uninterruptible finalizer that settles the assistant message and tool rows.
4. No unbounded in-memory collection keyed by session, tool call, or event.
   Every cache has a documented cap.
5. Static UI and API share an origin; `/api/*`, `/auth/*`, `/openapi.json` are
   never served by the SPA fallback.
