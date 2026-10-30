# 00 — Vision and scope

## What we are building

A single Rust binary `opencode` that is a drop-in **server** for the opencode
web client, CLI and TUI:

- HTTP JSON API + SSE event feed + WebSocket PTY attach, byte-compatible with
  `protocol/openapi.json` and the generated client types.
- Static hosting of the web UI on the same origin, SPA fallback, no TLS.
- The full agentic runtime behind those routes: sessions, agents, providers,
  tools, permissions, MCP, skills, commands, LSP, plugins, PTY, VCS, worktrees.

It is **not** a rewrite of the upstream TUI/desktop app, and it does not keep
the Effect runtime, the Bun server, the npm plugin supervisor or the Drizzle
layer.

## Hard requirements (from the product owner)

1. Fully agentic: permanent memory; the agent can author its own skills.
2. A goal system supervised by an **observer loop** (an LLM) that also
   supervises every agentic step, gets only terse summaries, and can repair
   sessions.
3. Full MCP, plugin, skill and LSP support.
4. Subagents:
   - not tied to one model; the main agent may pick a different model per
     subagent; a default subagent model is configurable;
   - siblings can talk to each other; non-siblings cannot;
   - subagents may spawn subagents when enabled, with a configurable depth limit;
   - parent (and authorized ancestors) can write into subagent sessions;
   - only the subagent's final output reaches the parent; details are pullable
     through a summarizing tool.
5. Observer receives compact flight strips — tool, rationale, expected seconds,
   intervene-after seconds — sees grandchildren, and repairs the smallest
   affected unit.
6. Build/test contention (Rust) is solved by a **big-brother/little-brother**
   model: a coordinator orders and serializes build/test lanes; siblings wait
   for tickets instead of each running `cargo test`.
7. Near-complete native opencode feature coverage.
8. Performance and RAM are first-class: budgets in [15-performance](15-performance.md).
9. Observer is an LLM model, can repair, and must notify every affected session
   and the UI when it does.

## Design consequences

| Requirement | Consequence |
|---|---|
| Permanent memory | SQLite + FTS5, not vector DB; memory injected as a cache-stable instruction source |
| Observer supervises everything | One observer actor per root session, fed by an expectation registry, not by transcripts |
| Terse observer input | Every tool call carries a mandatory `expectation` object; raw args/output are never sent to the observer |
| Sibling-only chat | Session tree authorization enforced in one place (`SessionPolicy`), not per tool |
| Subagent spawn limits | `depth` and `children` counters stored on the session row; no ancestor walks |
| Writable child sessions | Generalized `post(session_id)` guarded by ancestry + role; upstream `parentID === caller` check is replaced |
| Build contention | Command classifier + named lanes + filesystem locks; correctness does not depend on the LLM cooperating |
| RAM | No JS runtime by default, no embedding model, lazy LSP, bounded caches, `mimalloc` |

## Compatibility position

- Target upstream v2 protocol (the `packages/protocol` rewrite). V1-only events
  (`message.updated`, `message.part.*`) are not emitted. V1 config keys are
  accepted and normalized on load.
- Experimental routes are implemented only where the shipped web bundle or CLI
  calls them. Route presence is cheap; we keep handlers stubbed with the exact
  error shape where behavior is not yet implemented, but no route returns a
  fake success.
- Existing user data: we import upstream SQLite (v2) on first boot behind a
  flag. V1 migration is out of scope for phase 1 and revisited in phase 5.

## Non-goals

- Desktop (Tauri) shell, GUI plugin sandbox, updater.
- Workerd/Cloudflare deployment profile; the server is a local process.
- Multi-tenant auth. One password, one user, loopback by default.
- Reimplementing the TUI renderer.

## Definition of shipped

A feature is shipped when all four hold:

1. The route/event is byte-compatible (golden fixture test).
2. The behavior survives restart at every durable boundary.
3. The path has a measured idle-RSS and latency number in
   [15-performance](15-performance.md).
4. There is a scenario test that exercises it through HTTP, not a unit test of
   an internal function.
