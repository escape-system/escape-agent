# 16 — Roadmap and parallel execution

The port is executed in phases. Each phase ends with a runnable binary and a
scenario test through HTTP; no phase ships stubs or fake successes.

## Phase 0 — Docs and scaffold (current)

- This docs set; `oc-types` skeleton with ID/event/error types; workspace with
  all crates building; CI (`fmt`, `clippy -D warnings`, `test`); `oc-bench`
  skeleton; pinned UI build script; golden fixture harness.
- Exit: `cargo run -p oc-cli -- --version` works; CI green; docs reviewed.

## Phase 1 — Server foundation

Scope: `oc-types`, `oc-store`, `oc-config`, `oc-server`, `oc-cli`.

- SQLite schema + migrations + single-writer actor; session/message/part/inbox
  repositories; seq allocation; crash recovery.
- Config discovery/merge (V1 aliases), permissions evaluation, credentials,
  auth (Basic/cookie/pairing), CORS.
- axum router with the full route table present; handlers for `/api/info`,
  location, agent, session CRUD, messages, fs read/list/find, config, shell
  list, event SSE, static UI.
- SSE bus with one-serialization fanout, 4096 overflow close, heartbeat;
  durable event persistence + `/session/:id/log` replay.
- Static SPA serving with exact headers/fallbacks.

Exit: the upstream web app boots against the Rust server, lists/creates
sessions, reads/writes a file, streams `server.connected`; restart preserves
sessions; `oc-bench startup` and idle RSS gates pass.

## Phase 2 — Model and agent runtime

Scope: `oc-llm`, `oc-runtime`, `oc-tools` (core tools).

- Provider registry from models.dev snapshot + config overlay; OpenAI chat,
  Anthropic messages, Gemini; streaming SSE parser; tool schema conversion;
  retries/timeouts/overflow.
- Session actor, durable inbox, turn loop, prompt assembly with instruction
  hashes, compaction (summary; native optional), max steps, cancellation,
  crash recovery.
- Tools: read/write/edit/patch/glob/grep/shell/todowrite/question/webfetch/
  websearch; input repair; truncation; permissions ask/allow/deny flow.
- Agents: built-ins (build/plan/general/explore/title/summary/compaction),
  config agents, mode filters, model selection, system prompt replacement.

Exit: a real prompt runs end-to-end with a tool loop; the transcript and tool
cards render; interrupt leaves no half-committed state; permission modal round
trip works; provider switch mid-session preserves context.

## Phase 3 — Extensions

Scope: `oc-mcp`, `oc-lsp`, `oc-plugin`, skills/commands, VCS/worktree/shell/PTY.

- MCP: stdio, streamable HTTP, legacy SSE, WS attach, OAuth, tool namespacing,
  status events, lazy connect.
- LSP: supervisor, lazy spawn/eviction, diagnostics push+pull, `lsp` tools,
  edit/write diagnostics, status route.
- Skills: full discovery (all roots/compat), index, `skill` tool, `skill_write`,
  hot reload, commands.
- Plugins: native trait, JS sidecar with the bridge matrix, inventory/RPC/update
  routes, guarded ids.
- VCS diff/status/init; worktrees; shells endpoints; PTY via the upstream
  `process-daemon` crate port; persistent PTY tickets and WS attach.

Exit: an MCP server's tools run; a skill is authored and reloaded; rust-analyzer
diagnostics appear after an edit and are evicted after idle; a JS plugin's
`tool.execute.before` runs via the sidecar; terminal opens over WS.

## Phase 4 — Agentic differentiators

Scope: `oc-observer`, `oc-memory`, `oc-tools` (agent tools), UI patches.

- Expectation injection + registry + observer actor; terse strips; nudge,
  inspect, repair; budgets; repair notifications; goal table/judge/tools.
- Memory store, FTS retrieval, instruction source, scribe, memory tools.
- Subagents: model resolution chain, depth/child limits, sibling policy, `talk`,
  `roster`, `session_write`, output-only results, `digest`, tree API.
- Lanes + coordinator agent + the `rust-build-test` skill; classifier and
  semaphores.
- UI: five settings tabs, subagent tree, peer drawer, observer timeline, goal
  chip, lane pill.

Exit: the scenario in [05](05-subagents.md) (20 subagents, one stalls; observer
nudges/repairs; sibling messages; build lanes serialize) runs through HTTP and
is visible in the UI; memory survives restart and is injected; a goal is judged
done only with evidence.

## Phase 5 — Parity and hardening

- Experimental routes: schedule, generate, voice, image_generate, Code Mode
  (`execute`), migration/status, debug, usage, stats/export/import.
- Import of upstream v2 DB; V1 decision documented with a migration report.
- OpenAPI served and diffed against upstream; fixture suite for every route.
- Fuzz: SSE parser, provider chunks, patch parser, config JSONC, LSP framing,
  MCP framing.
- Perf: benchmark all gates on Windows+Linux; 100 sessions/20 locations; leak
  tests over 24 h.
- Security pass: path traversal, symlinks, SSRF on webfetch, command injection
  in shell selection, plugin trust, credential exposure.

Exit: near-complete feature matrix green; no open P0/P1 security findings; all
performance gates met.

## Parallel workstreams (10–20 agents)

Fan-out only after interfaces are frozen by the integration owner.

| Wave | Workstreams | Owns |
|---|---|---|
| 1 | A: store/migrations; B: protocol types + fixtures; C: config/permissions/auth; D: server skeleton/SSE/static; E: CLI/service lifecycle | crates disjoint |
| 2 | F: providers; G: turn loop/inbox; H: prompt/compaction; I: core tools; J: filesystem/search/VCS | `oc-llm`, `oc-runtime`, `oc-tools` split by module |
| 3 | K: MCP; L: LSP; M: plugins/sidecar; N: skills/commands; O: PTY/shell/worktree | one crate/domain each |
| 4 | P: subagents/siblings; Q: lanes; R: memory; S: goals/observer; T: UI | observer and subagents share `oc-types` contracts; UI after endpoints |
| 5 | U: experimental routes; V: import/OpenAPI; W: perf/fuzz/security | cross-cutting, sequential reviews |

Rules:

- Every workstream has a named contract (types/routes) in `oc-types` or
  `oc-testkit` before it starts; changes require the integration owner.
- Agents never edit the same file; shared edits are coordinated via the owner.
- No agent runs build/lint/test mid-flight; the integration owner runs the full
  gate once per wave.
- Each wave ends with a scenario test through HTTP, not just unit tests.

## Risk register

| Risk | Impact | Mitigation |
|---|---|---|
| Effect encoding semantics differ (`absent` vs `null`) | silent UI breakage | golden fixtures; no optional fields serialized unless set |
| Provider protocol drift | turns fail | protocol tests against recorded streams; per-provider compatibility flags |
| SQLite writer bottleneck under many sessions | latency | one tx per durable event, WAL, prepared statements, benchmark 50 concurrent |
| Cancel loses final events | corrupt transcripts | uninterruptible finalizer + crash-recovery tests |
| JS sidecar RAM | budget miss | lazy, 90 MB cap, no plugins ⇒ no process |
| LSP RAM | budget miss | no auto-download, caps, eviction, heavy-slot accounting |
| Observer loops / cost | runaway spend | coalescing, budgets, no transcript, fail-open |
| Sibling message storm | CPU/token burn | per-sender rate limit, queue, policy deny |
| Build lane bypass | locked workspace | classifier + OS/semaphore enforcement, not model cooperation |
| UI patch drift | build breaks | pinned commit + numbered patches + CI build |

## Definition of done (whole port)

1. Every route in [02](02-protocol.md) returns the exact envelope/status or a
   documented, deliberate 501 with the right error shape.
2. The upstream web app, CLI and TUI work against the Rust binary with no
   console errors.
3. All performance gates in [15](15-performance.md) pass on Windows and Linux.
4. Crash/restart at every durable boundary is tested.
5. The agentic features in [05](05-subagents.md), [06](06-memory.md),
   [07](07-skills.md), [08](08-goals-observer.md) are exercised end-to-end and
   visible in the UI.
