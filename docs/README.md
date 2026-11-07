# opencode-rust — Documentation

This directory is the source of truth for the Rust server port. It is written
before code on purpose: the port is large, and every design decision below has a
named owner, an invariant, and an acceptance check.

Upstream reference: `../opencode` (opencode v2 rewrite — `packages/{schema,core,
protocol,server,client,app}`). The port targets the **server** surface that the
web client, CLI and TUI consume over HTTP/SSE/WebSocket. TUI rendering is out of
scope; the TUI must keep working against our server.

## Read in this order

| # | Doc | What it locks down |
|---|-----|--------------------|
| 00 | [vision](00-vision.md) | Scope, non-goals, product requirements, budgets |
| 01 | [architecture](01-architecture.md) | Crate graph, processes, concurrency, data flow |
| 02 | [protocol](02-protocol.md) | HTTP routes, envelopes, SSE, events, errors, compat |
| 03 | [storage](03-storage.md) | SQLite schema, invariants, replay, crash recovery |
| 04 | [agent runtime](04-agent-runtime.md) | Turn loop, prompt assembly, compaction, retries |
| 05 | [subagents](05-subagents.md) | Models, siblings, spawning, writable sessions, lanes |
| 06 | [memory](06-memory.md) | Permanent memory, retrieval, auto-scribe, self-skills |
| 07 | [skills](07-skills.md) | On-disk format, discovery, self-authored skills |
| 08 | [goals & observer](08-goals-observer.md) | Expectation contract, observer loop, repair |
| 09 | [MCP](09-mcp.md) | stdio/HTTP/WS transports, OAuth, tool mapping |
| 10 | [plugins](10-plugins.md) | Native host + JS sidecar, hooks, transforms, GUI |
| 11 | [LSP](11-lsp.md) | Lazy servers, diagnostics, tools, eviction |
| 12 | [tools](12-tools.md) | Tool contracts, expectations, truncation, lanes |
| 13 | [UI](13-ui.md) | Static hosting, settings extensions, subagent UI |
| 14 | [config & permissions](14-config-permissions.md) | Config keys, precedence, permission flow |
| 15 | [performance](15-performance.md) | Budgets, hot paths, allocation rules |
| 16 | [roadmap](16-roadmap.md) | Phases, parallel workstreams, acceptance |
| 17 | [interfaces](17-interfaces.md) | Landed public APIs, ownership, agent coordination |
| 18 | [status](18-status.md) | Live implementation snapshot and verification matrix |
| 19 | [agentic integration](19-agentic-integration.md) | Memory/observer/subagents/lanes wiring plan |
| 20 | [engineering guide](20-handoff.md) | Architecture state, configuration, and verification |
| 21 | [grouping](21-grouping.md) | `metadata.group` label convention, rejected alternatives |
| 22 | [audit report](22-audit-report.md) | Verification audit, defect resolutions and regression tests |
| 23 | [release](23-release.md) | Tag-triggered builds, archive layout, version-must-match rule |

## Rules that apply to every doc

- Wire compatibility is defined by `../opencode/packages/protocol/openapi.json`
  and `../opencode/packages/client/src/promise/generated/types.ts`, **not** by
  this prose. When they disagree, the generated types win and this doc is fixed.
- Optional fields are omitted, never `null` (Effect `optional()` semantics).
- IDs are prefixed strings (`ses_`, `msg_`, `evt_`, `prt_`, `req_`); IDs are
  opaque to clients and must never be re-derived except `evt_` → `msg_` for
  event-backed messages.
- No feature is "done" without a restart test: kill -9 the server mid-turn and
  verify recovery from durable state.
