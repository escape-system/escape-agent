# 18 — Implementation Status & Verification

This document provides the technical status, architectural verification logs, and active roadmap for the Escape Agent (`opencode-rust`) workspace.

## v0.3.0

- **Stop / background children** — prompts queued for a background spawn before the actor starts are withdrawn by `interrupt` (`interrupt_before_a_background_child_starts_withdraws_its_prompt`, `crates/oc-runtime/tests/nested_spawn.rs`).
- **Durable UI stop state** — the client keeps a per-session stop request (`requestStop`/`isStopRequested`) that survives stray `session.execution` / running events and clears on execution start or a terminal event; new client tests in `packages/client/test/solid-stop-request.test.ts`; UI patch `0006-stop-interrupt.patch` regenerated.
- **Content-free box tool metadata** — `box_list`/`box_write` return summaries only; `box_read` keeps the body in its tool output, so box bodies no longer ride into metadata or prompt context.
- **Permission-filtered `box_list`** — rows are filtered per key through `box:read`, so a `secret*` deny hides both the rows and their count.
- **Subtree-scoped temporary-box expiry** — compaction expires temporary boxes of the compacting subtree only, leaving sibling/child trees intact.
- **Index truncation notice** — the prompt index reports truncation instead of silently dropping rows; empty box bodies are rejected.
- **Observer in the server harness** — `crates/oc-server/tests/runtime` wires the real observer engine and proves a user-stopped session is never re-raised (16/16).
- **Windows** — the workspace builds for `x86_64-pc-windows-gnu` (ConPTY/shell/config already cfg-gated). The release workflow publishes a Linux archive only; Windows binaries are produced by cross-compiling locally (`cargo build --release --target x86_64-pc-windows-gnu`) and attached to the GitHub release by hand until the workflow gains a matrix.

## Snapshot

**CI Gates are green.** `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings` pass cleanly. `cargo test --workspace` reports **803 automated tests across 80 suites (0 ignored, 0 failed)** covering unit, integration, and end-to-end scenarios.

The codebase adheres to strict modularity: large source and test files exceeding 1,000 lines were split into cohesive submodules behind clean facades, preserving public APIs, serde wire shapes, routes, and error envelopes (see [17 — Interfaces](17-interfaces.md#module-layout-oversized-file-split)).

All behavior tests verify end-to-end semantics: runtime steer FIFO/recovery/compaction reasons, server form validation/export sanitize/import rejection, shell UTF-8 cursor and session env, worktree confinement, cron/duration parsing, permission precedence, and MCP/LSP framing.

Smoke verification passes on isolated ports and databases across core surfaces: UI serving, SSE streaming deltas, session lifecycle, provider-less truthful failures, shell process capture, and `oc-cli` binary-level E2E suites (text turns, tool turns, restart durability, static assets, and permission prompts).

## Verified

| Area | State |
|---|---|
| Workspace | 17 crates, edition 2024, `rust-version 1.90`; fmt/clippy clean, 803 tests / 80 suites / 0 ignored / 0 failed, `oc-cli` builds |
| `oc-types` | full wire model (session/message/inbox/permission/agent/file/config/location/info/error) + typed event payloads (`SessionViewed` included; fork/form/instruction/skill payloads are typed locally in the runtime/server) |
| `oc-store` | SQLite WAL, writer thread + 4-connection read pool, sessions/messages/inbox/events, dense-seq projection, memory FTS, goals/observations/repairs, subagent tree, forms, schedules, snapshots, instruction entries, saved permissions, lane tickets, credentials, worktrees, projects; migrations `0001`–`0010`; `register_project` upserts the `LocationRef` project on `GET /api/location` |
| `oc-store` (context boxes) | migration `0011_context_box` adds `context_box` keyed `(root_id, key)`: create-or-update writes with in-transaction append, soft delete, `stable`/`temporary` kinds, `time_expired` audit stamp with revival, revision bumping, paging; a deleted row is recreated with `revision = 1`. Six store tests (`crates/oc-store/tests/store_ext/boxes.rs`): tree sharing, revision/append, validation, expiry/revival, delete/recreate, paging. Store suite 21/21 |
| `oc-config` | discovery/JSONC/V1 aliases, agents, permissions, skills, commands, AGENTS.md, rescan; the `explore` subagent can read-only `code_map` (`allow("code_map","*")`, `crates/oc-config/src/agents.rs:294`; regression `explore_toolset_allows_read_only_code_map_and_still_denies_edits`, `crates/oc-config/tests/config.rs:543`); 37 tests |
| `oc-llm` | embedded models.dev snapshot, catalog/resolve/overlay, OpenAI/Anthropic/Gemini streaming, retries |
| `oc-runtime` | session actors, turn loop, tools + permissions, armed threshold compaction (`compaction.auto` defaults **false**, but the guard is armed when `auto` is true **or** `compaction.ceiling_tokens` is set and fires above `ceiling − buffer`; the internal reason is `guard` when `auto` is off and the wire reason stays `auto`; overflow recovery and manual `/compact`/`compact_context` unchanged; ceiling is `context_window − max(15%, 16k)` with `compaction.ceiling_tokens` able only to lower it; manual inbox `compaction` rows write `reason:"manual"` and a failed inbox delete stops the actor instead of re-dispatching), step budget (`session.execution.step_limit` v1 `{sessionID, steps}`, published after `session.execution.succeeded` and never as a terminal; parent metadata carries `stepLimit`/`steps`), `BoxTools` (`box_list`/`box_read`/`box_write`/`box_delete`/`compact_context`) with `box:read`/`box:write` mappings and key resources, `oc/boxes` prompt index pinned per actor epoch with tail supplements, temporary-box expiry after the `completed` row commits, mid-turn `steer` promotion at step boundaries (repost waits for idle; `switch_model` rejects a catalog-unknown target), cancellation/recovery, memory, observer strips, `ContextPressure`/`context_pressure()` with the host-side compaction adviser (`observer.advised`; the adviser lives in `oc-runtime`, not `oc-observer`), subagents (nested spawn for `general`, digest scoped to the caller's subtree with the child's full final answer and raw-answer cache honoring each caller's token budget, bounded watcher that cancels on timeout and delivers failed/timeout exactly once, parent cancellation interrupts a foreground child through `ToolContext.cancel`), `ToolProvider`, `GoalTools`/`SkillTools`/`LaneTools`, `ProcessLaneRunner`, lane events, pre-write snapshots, stored-credential key injection (active first, then any); actor `Err` finalization: a drain-loop error repairs the rows the dead turn left open and then publishes `session.idle` + `session.execution.failed` through a per-item `TerminalState` guard so a settle that already ran is not duplicated, and the actor retires from the map and respawns when durable input is already waiting (`crates/oc-runtime/src/runtime/actor.rs:185`); regressions `actor_error_publishes_one_terminal_pair_and_closes_the_rows` (`crates/oc-runtime/tests/runtime/actor_error_terminal.rs`) and `crates/oc-runtime/tests/runtime/context_boxes.rs` (5 tests: tree sharing, `compact_context` enqueue, explore read-only / write-only agent, two-compaction survival with temporary expiry, cache-stable prefix with tail supplement); 91 runtime-scenario tests + 68 lib tests |
| `oc-tools` | `code_map`, `read`, `write`, `edit`, `patch`, `glob`, `grep`, `shell`, `todowrite`, `webfetch`, `websearch` (72 tests); registry merge, `&str` names; `ToolContext.cancel` (`CancellationToken`) lets long-running tools stop detached helpers when the execution is interrupted; `code_map` `map` delegates to ripwire whenever the binary is found (mode-dependent argv: empty query `--max-tokens`/`--top-k`, query `--for`/`--token-budget`), captures stderr into `metadata.ripwire_stderr`, enforces a 30 s timeout (`OPENCODE_RIPWIRE_TIMEOUT_MS`) as a hard error, and discovers `RIPWIRE_PATH` → `$HOME/.local/bin/ripwire` → `PATH` requiring a regular executable file (`crates/oc-tools/src/code_map.rs`); hermetic delegation tests in `crates/oc-tools/tests/code_map_ripwire.rs` |
| `oc-observer` | registry, terse input, strict verdicts, budgets, goal state machine, repairs; judge credentials resolved per request from the credential store or provider env (declared-but-unset env no longer disables it), failures publish `observer.unavailable`; goal verdicts persist to the store before `goal.updated`; switch_model falls back to `observer.escalation_model`, repost replays the last user prompt (with a pending-input guard), restart_subagent re-enqueues the original child prompt; the context-pressure adviser is host-side in `oc-runtime` (`observer/advice.rs`), not in this crate |
| `oc-subagents` | session-tree policy, spawn limits, sibling/coordinator scope, write policy, model chain, budgets |
| `oc-lanes` | cargo/node/test classifier, priority lanes, semaphores, cancel/reorder/wait, max queue; boot restore (`RestoreError`, `lane_tickets_all`) replays queued tickets, fails `running` ones with a restart reason and never reuses an id; `enqueue_detailed` + `tree_rev` coalescing serves one run to many subscribers, in memory only |
| `oc-memory` | scoped retrieval/render/budget, secret detector, tools, scribe library; the CLI attaches an LLM `ScribeModel` (`subagents.digest_model` → `observer.digest_model` → `observer.model`), so `digest` and turn-end auto-scribe work, gated by `memory.auto_scribe` |
| `oc-mcp` | stdio + streamable HTTP, handshake/tools/resources/prompts, OAuth PKCE helpers; manager cache + `McpToolProvider` |
| `oc-lsp` | supervisor, stdio framing, sync, push/pull diagnostics, tool ops (`lsp`, `lsp_diagnostics`), eviction/shutdown, `LspToolProvider` |
| `oc-plugin` | native registry/domains/hooks, JSON-RPC stdio sidecar, inventory/RPC |
| `oc-pty` | in-process ConPTY/PTY, 1 MiB ring, rows/snapshot/replay |
| `oc-server` | HTTP/SSE/WS, auth/pairing (password from `--password`/`OPENCODE_PASSWORD`/`OPENCODE_SERVER_PASSWORD`; explicit empty disables; Basic/session token/`auth_token`/same-origin cookie; static shell public so the connect screen loads), static app, sessions/messages/inbox/log, forms, fork/revert/context/diff/view/environment, export/import/stats/instructions, permissions (request + saved; `always` persists to `permission_saved`, auto-resolves covered asks, `reject` drains every pending ask of the session, saved rules use wildcard matching and answer the admission route too), builtin `compress` command + raw `/compress` prompt alias, model/provider (only available providers), config, fs (list can navigate outside the location for the picker; reads/finds stay confined), vcs, project, shell manager, persistent PTY, MCP/LSP/plugin/RPC, lanes, observer, goal, memory, subagent, credential/integration (catalog providers connectable via key+env), worktree (refresh takes the main tree from git's first porcelain record, excludes the project's own checkout and the main tree, self-heals stale rows), duplicate synthetic message ids are 409, default location resolution does not double a relative server directory, static directory paths are 404 not 503; 230 tests |
| `oc-cli` | `serve` with store/config/catalog, password resolution (flag/env alias, empty disables, service reuses registration or generates), `service get <url|password|pid|id|version>`, runtime + LLM scribe attach (model chain normalizes null/empty `subagents.digest_model` so it still falls through to `observer.digest_model`/`observer.model`), memory, observer host, MCP/LSP, PTY, plugins, lanes, service registration |
| `oc-testkit` | fake provider/process harness + binary-level scenarios |
| UI | vendored SolidJS source (`ui/vendor/opencode`, MIT) built into `web/dist`; **Escape Agent brand layer** (dark default, gold/charcoal `oc-2` theme, glass panels/composer/tabs, centered home hero, six-letter wordmark + matching pin grid, branded connect/about, favicon/PWA/brand assets); settings tabs (memory, goals, observer, subagents, lanes) and session agent panels (subagent tree, peer drawer, observer repairs/budget, goal chip, lane pill); models with `time.released = 0` stay pickable (`0003`); missing tool metadata no longer crashes the session page (`0004`); `/compress` appears in the slash menu from `GET /api/command`; connect/login screen appears on a 401 and logs in with the configured password; live Playwright readiness suite in `packages/app/e2e/escape-agent` (**6/6** on 4096 + password 4097) |

### Wave: code_map/ripwire, explore toolset, actor finalization

Per-crate counts below were verified in this wave; they are not a new
workspace total, and the `803` snapshot above predates these entries.

- **`oc-tools` rewrite.** `code_map`'s `map` operation delegates to `ripwire`
  whenever discovery finds an executable binary — including an empty query,
  which previously fell back to the native engine. The argv is
  mode-dependent: empty query sends `--max-tokens=<N> --top-k=<limit>`,
  a query sends `--for=<query> --token-budget=<N>`; ripwire 0.6.8 silently
  ignores `--max-tokens`/`--top-k` under `--for` (measured: identical 6825 B
  output for 300/1200/absent and a stderr warning), so those flags are omitted
  in that mode (`crates/oc-tools/src/code_map.rs:599-644`). stderr is captured
  and surfaced as `metadata.ripwire_stderr`, exit 0 with stderr is still
  success, and the 30 s timeout (`OPENCODE_RIPWIRE_TIMEOUT_MS`) is a hard
  error rather than a masked native fallback (`:409-465`, `:662-765`).
  Discovery is `RIPWIRE_PATH` → `$HOME/.local/bin/ripwire` → `PATH`, each
  candidate required to be a regular file with the executable bit
  (`:852-924`). The install tip and `ripwire_install_command` metadata are
  emitted only when no binary was found (`:581-593`). New hermetic test
  `crates/oc-tools/tests/code_map_ripwire.rs` re-execs its own binary to
  control `RIPWIRE_PATH`/`HOME`/`PATH`/timeout without `set_var`, and asserts
  the measured argv contract, stderr surfacing, timeout error, and install-tip
  gating. `oc-tools` carries 72 tests.
- **`oc-config`.** The `explore` subagent now allows the read-only `code_map`
  tool (`crates/oc-config/src/agents.rs:294`, with the comment explaining its
  action is the tool name, not `edit`). Regression
  `explore_toolset_allows_read_only_code_map_and_still_denies_edits`
  (`crates/oc-config/tests/config.rs:543`) also pins that
  `edit`/`write`/`patch` stay denied. `oc-config` carries 37 tests.
- **`oc-runtime`.** The drain-loop `Err` path no
  longer just marks the session idle and exits: `actor_loop` calls
  `finalize_actor_failure` (`crates/oc-runtime/src/runtime/actor.rs:185-192`),
  which repairs the assistant/compaction rows the dead turn left open and then
  publishes `session.idle` followed by `session.execution.failed`
  (`crates/oc-runtime/src/turn/mod.rs:637-706`). A per-item `TerminalState`
  guard (`crates/oc-runtime/src/runtime/actor.rs:56-96`) makes the path
  idempotent with a settle that already ran, and `drain` arms it only for real
  executions, not `compaction` rows (`:222-226`). The actor also retires from
  the map and respawns itself when durable input is already waiting
  (`:206-211`, `:242-273`). Regression
  `crates/oc-runtime/tests/runtime/actor_error_terminal.rs::actor_error_publishes_one_terminal_pair_and_closes_the_rows`
  pins that a panicking tool publishes exactly one `session.idle` then one
  `session.execution.failed`, closes the dead assistant/tool rows, and the next
  prompt is served by a respawned actor; `crates/oc-runtime/tests/runtime/actor_death.rs`
  covers the respawn half. `oc-runtime` carries 178 tests.
- **Docs.** `docs/04-agent-runtime.md` gained "Guards and finalization (Hermes
  Agent ports)" describing verify-on-stop (`turn/verify.rs`), the stall guard
  (`turn/stall.rs`), the repetition guard (`turn/repetition.rs`) and the shell
  hardline (`oc-tools/src/shell_guard.rs`). `docs/17-interfaces.md` fixed
  `ids.rs`→`id.rs` and the `ToolContext` drift (added `cancel`).
  `docs/07-skills.md` attributes the `skill_lint` soft budget
  (`crates/oc-runtime/src/skill/lint.rs:15-16`) and its warning/error split.
  `docs/14-config-permissions.md` now lists `code_map` in the `explore`
  toolset.

### Wave: code_map graph engine (lazy ripwire MCP pool + cached native index)

Workspace test suite in this wave: **1,140 passed, 0 failed, 3 ignored**
(1,137 unit/integration + 3 doctests passed; 3 pre-existing doctests ignored).

- **`oc-tools` (`crates/oc-tools/src/code_map/`)**: Expanded `code_map` from 4
  to 12 operations (`map`, `outline`, `pack_task`, `communities`, `symbol`,
  `expand`, `references`, `impact`, `connect`, `affected`, `trace`, `batch`),
  plus optional `rank_by`, `start_line`, `end_line`, `compress`, and `queries`
  parameters (`strict`/`additionalProperties: false`, no `oneOf`/`anyOf`).
  Added a lazy bounded `ripwire <root> --mcp` stdio JSON-RPC pool in
  `ripwire_mcp.rs` (2 daemons per tokio runtime, cross-runtime cap 16, 300 s idle
  eviction, 1x `ripwire://legend-dict/full` schema read, `CliOnly` binary
  capability caching, hard timeout/cancel kill-and-evict, broken-pipe 1x
  respawn), glossary-comment compaction and `analyze` XML budget packing in
  `ripwire.rs`, and an mtime+size-cached pure-Rust symbol+call-graph fallback in
  `native_index.rs`. Omitted `limit` on MCP `for`/`explore` calls so ripwire 0.6.8
  returns full `<ctx>` bundles rather than 1-row `candidate-page/v1` summaries.
  `oc-tools` carries 94 tests (`code_map_ripwire.rs` covers CLI contract,
  persistent MCP pool + all 12 ops, and pure-Rust native fallback).
- **`oc-runtime` & `oc-config`**: Strengthened graph-first `code_map` workflow
  guidance in `crates/oc-runtime/src/prompt/mod.rs` (removing the hardcoded
  install URL from the cached prompt prefix), added `"code_map"` to fast
  inspection expectations (`crates/oc-runtime/src/expectation.rs`), and updated
  `PROMPT_EXPLORE` (`crates/oc-config/src/agents.rs`) to start with `code_map`
  before `grep` or full-file `read`. `oc-config` carries 41 tests;
  `oc-runtime` carries 268 unit/integration tests + 1 doctest.
- **Docs**: Updated `docs/12-tools.md` (`code_map` 12-operation contract) and
  `docs/17-interfaces.md` (`oc-tools` `code_map/` module layout).

- **Sessions**: list/create/get/patch/delete/active, agent/model/move,
  interrupt/background, prompt/synthetic/compact/command, fork,
  revert stage/commit/clear (real pre-write snapshots), context, diff
  (snapshot vs current tree), environment,
  view, messages, inbox, log replay, wait.
- **Forms**: `GET/POST /api/session/:id/form`, get/reply/cancel, plus
  `GET /api/form` across sessions. Durable in SQLite (not the upstream 10-minute
  in-memory TTL). Lifecycle events are ephemeral
  (`form.created`/`form.replied`/`form.cancelled`).
- **Permissions**: pending/reply/evaluate routes, saved-rule list/patch/delete.
- **Goal/skill/lane/MCP/LSP/plugin**: `/api/experimental/{goal,observer,
  subagent,memory,lane}`, `/api/mcp*`, `/api/lsp*`, `/api/plugin*`,
  `/api/rpc/:rpcID/:method`. MCP config mutation is implemented
  (`PUT`/`DELETE /api/experimental/mcp/:server`).
- **Schedules**: `/api/experimental/schedule` CRUD + pause/resume, durable in
  the `schedule` table and fired by the CLI runner. `when` supports `in`,
  `at`, `every` and five-field `cron`; durations accept seconds or
  `<n> <unit>` pairs; `ends` supports `after`/`at`/`times`. Time zones are
  `UTC` or numeric `±HH:MM` only — IANA names are rejected with 400.
- **Shell manager**: `GET/POST /api/shell`, `GET/DELETE /api/shell/:id`,
  `GET /api/shell/:id/output`, `POST /api/session/:id/shell`, session terminals
  and persistent PTY aliases.
- **Credential/integration**: full credential CRUD + activate; integration
  list/get lists every non-disabled catalog provider as connectable (`key` +
  `env`); `key`/`external` connects persist credentials; the runtime injects the
  active stored key (falling back to any matching credential) into the provider
  client; OAuth and command engines are honestly `501`.
- **Models/providers**: `/api/provider`, `/api/model` and `/api/model/default`
  expose only available providers — explicitly configured in `providers`,
  backed by a stored credential, or reachable through a declared env var
  (disabled providers are never available).
- **Worktree**: `GET/POST/DELETE /api/worktree`, `POST /api/worktree/refresh`,
  backed by real `git worktree` invocations with path confinement and output
  caps.
- **Filesystem/VCS**: `GET /api/fs/list` may navigate absolute/relative paths
  outside the location (including parents and other drives) so the web
  directory picker works; `fs/read` and `fs/find` remain confined. Experimental
  write/watch, vcs info/base/status/branch/diff/init, project list/patch, and
  `GET /api/location` registering the project row on first use.
- **PTY**: CRUD, ticket WS upgrade, snapshots.

### UI

`web/dist` is built from the patched upstream app
(`ui/patches/0001-settings-tabs.patch`,
`ui/patches/0002-session-agent-panels.patch`,
`ui/patches/0003-model-release-unknown.patch`,
`ui/patches/0004-background-metadata-guard.patch`). The settings tabs and the
session agent panels are present in the built bundle, so the server serves them
at `/`.

## Live verification (2026-10-30)

### Feature wave (binary 15:07:06, gate 773/78)

- **Password-protected web UI**: `--password` / `OPENCODE_PASSWORD` /
  `OPENCODE_SERVER_PASSWORD`; explicit empty disables auth; `--service` reuses
  the registered password and `opencode service get password` retrieves it.
  Live matrix 24/25 (the one miss was a bare `/_assets` directory answering
  503; fixed to 404 and re-smoked). Browser: 401 → `ConnectServerScreen` →
  password login → home, no console errors. Static shell stays public by
  design; every `/api` route is 401 without a credential.
- **`/compress`**: builtin command in `GET /api/command` (appears in the web
  slash menu) + `POST /command {"name":"compress"}` + raw `/prompt
  {"text":"/compress"}` alias (case-insensitive, whitespace-tolerant); a
  markdown `compress` command overrides it. Live 7/7 groups: completed/failed
  `reason:"manual"`, one attempt on terminal 500, queue waits for turn settle,
  `POST /compact` unchanged, no user message admitted, config override renders
  its template instead.
- **Real-model `/compress`** (4131, river): command and alias each produced one
  compaction `completed`/`reason:"manual"` with a non-empty summary (609/689
  chars); a queued `/compress` admitted during generation ran only after the
  turn settled (10 ms after idle) with a 1294-char summary.
- **Vendored frontend**: `ui/vendor/opencode` (5,288 source files, 54 MB,
  MIT) plus `ui/build.sh`/`build.ps1` that install frozen, build
  `@opencode/app`, replace `web/dist`, and run `ui/verify-dist.mjs`. Two
  consecutive builds and the PowerShell path exited 0; `web/dist/index.html`
  is 9,608 B with no placeholders. Editing `packages/app/index.html`/`src/**`
  and rebuilding is the supported workflow; no sibling checkout needed.
- **Microscopic route matrix**: 112 route calls, 38 shape assertions; no 2xx
  error body, no HTML under `/api`, no fake success; unknown `/api` is a JSON
  404. `/compress` prompt returns a compaction item and no user message.
- **Subagents/background/digest/siblings**: 10/10 (foreground/background
  exactly-once, live `POST /background` settles the parent while the child
  later delivers one synthetic, nested depth 2 with `explore` keeping its
  `subagent` deny, timeout delivers one failure and drops the permission ask,
  parent interrupt stops the child in 0.01 s, digest keeps a 1,161-char final
  answer complete and honors a second `max_tokens` cache hit, sibling
  note/question/answer/SSE/session_write/disabled/deleted-target contract).
- **Sessions/messages**: 39/39 sub-checks (CRUD, pagination/cursor, steer vs
  queue invisibility/once, inbox delivery change/cancel, log replay exact
  seq range, active map, synthetic duplicate 409, fork, revert stage/clear/
  commit with real snapshots, context/diff/view/environment, export/import,
  stats/instructions, no `null` optional fields in 378 responses).
- **Observer/goal/memory/schedule**: 17/17 (check 204/404 and no 401s; goal
  persist-before-single-`goal.updated`; compact/switch/repost/restart repairs
  with the running-turn repost guard and catalog-unknown model rejection;
  project/session memory FTS/delete/secret rejection; scribe enabled vs
  disabled; a 2 s schedule fired exactly once and pause/resume/delete held).
- **Schedule/lane/shell/PTY**: all pass (schedule CRUD and `when` parsing
  including numeric tz and IANA 400; lane tickets/SSE/cancel/reorder; shell
  manager output cursor/truncation and session shells; PTY CRUD, ticket-gated
  WS replay, single-use tickets). Lane enqueue is tool/SSE-driven by design
  (no HTTP enqueue route); unclassified commands run inline.
- **Other groups**: worktree 15/15 + fork/revert/export/import 34/34 (fork
  links via the durable `session.forked` event with `parentID:null` by
  design); MCP/LSP/plugin 20/20 (plugin update is the documented truthful
  503); permissions once/always/reject-batch/restart/wildcard/deny-precedence
  all pass; auth pairing/token/cookie/SSE matrix passes.
- **Location fix**: a relative `--directory` no longer doubles in implicit
  location resolution (`work/work`); `serve` stores an absolute directory and
  default resolution joins `.`. Live: `POST /session` without a location on a
  relative-directory server returned the exact absolute path and created the
  project.
- **Static directory paths**: bare `/_assets`/`/icons` now 404 instead of the
  false "Web UI is not built" 503; assets under them remain immutable 200.

### Regression & Edge-Case Verification

Every slice below ran on the 14:02 binary with isolated ports/DBs; the user DB
and the 4096 server were not modified except for read-only UI checks.

- **Real provider, user's verbatim prompt** (two trials, isolated DB): the
  context is 1,024,000 and there was **no exploration tool before `subagent`**
  in either trial. Trial 1 planned first (`goal_set`+`todowrite` at 13 s, four
  parallel `subagent` calls at 28.3 s); trial 2 made `subagent` the literal
  first tool at 51.3 s. Depth 2 was reached at 167.7 s (root → 4 depth-1 →
  explore depth-2); interrupt settled the root and all 5 descendants with zero
  provider errors. The old 167 s exploration delay does not reproduce; strict
  "first tool" is model-dependent, "before exploration" holds.
- **Subagent failure modes + live `POST /background`** (ports 4102/4103):
  **6/6 PASS**. Timeout delivered exactly one failed synthetic in 2.3 s; deleted
  child, provider 500 and unanswered permission Ask all settled once; background
  completion was exactly-once after 3 s. New scenario F: the root's foreground
  `subagent` tool returned `{status:"running"}` 311 ms after the prompt, the
  root settled at 313 ms, the detached child finished at 2.06 s and delivered
  **one** synthetic, waking exactly one extra execution.
- **Steer/queue/interrupt** (ports 4104/4105): **A–E PASS**. Steer was delivered
  at a step boundary (`inbox.delivered` before the next `step.started`) and
  drove exactly one request/execution; queue stayed inbox-only while the turn
  ran, was promoted after settle, and drove exactly one execution. Interrupt
  abandoned the stream in 56 ms; kill -9 recovery preserved the aborted step.
- **Permission `always` + restart + wildcard** (ports 4106/4107): **all checks
  PASS**. The `psv_` rule survived a real process restart (new PID, same DB) and
  was honored by both the tool-ask path and `POST /api/session/:sid/permission`
  (`effect:"allow"`, no pending); deleting it restored `ask` on both paths. A
  saved `*.env` rule covered `other/.env` and `config/.env`, not
  `config/.env.local`.
- **Manual compaction** (4132/4135): **4/4 PASS**. `POST /compact` wrote a single
  `compaction` row `status:"completed" | "failed"`, `reason:"manual"`, consumed
  the inbox row, produced no assistant turn, and a terminal provider 500 was
  attempted exactly once (no retry loop after 2 s).
- **Worktree** (4133, real git): refresh returned `[]` for a clean repo, listed
  a linked worktree exactly once, stayed idempotent across three refreshes, and
  healed after the worktree disappeared. A project rooted at a linked worktree
  excludes both the repository's main tree and its own checkout. Missing
  `projectID` is a 400; DELETE of an unregistered main path is an honest 404.
- **Observer repairs** (4100/4101): **5/5 PASS** (compact, switch_model,
  repost, restart_subagent, goal_done persisted before one `goal.updated`); no
  401s, `observer/check` 204. Guard probes: a catalog-unknown `switch_model`
  target is rejected without persisting it, and a repost arriving while a turn
  runs is skipped.
- **Siblings** (4112/4113): **8/8 PASS** with a steer-aware harness script. The
  unmodified old harness stalls because it scripts the provider off the last
  user message and assumed background completions arrive only at idle; the new
  intended behavior delivers them mid-turn as `Steer` (the fix), so the harness
  was adjusted, not the product.
- **UI (4096, read-only)**: context **1,024,000**; model picker lists and
  selects `DeepSeek V4.1 Flash Max`; settings → Models lists the model; the
  user's session page renders the 10-session tree (3 depth-1 + 6 depth-2) with
  no `shellID` TypeError. Only the known non-git worktree-refresh request noise
  appears in the console.

- **Pre-fix history (earlier builds).** **Real provider, real model**
  (`river/deepseek-ai/DeepSeek-V4.1-Flash-1017`):
  an implicit "spawn a subagent" prompt called `subagent` first and created a
  working child (tools, file effects, synthetic + `peer` delivery). With the
  user's verbatim wording the model explored for **167 s** before its first
  spawn; the reported session was interrupted at 113 s, before that point. The
  tool was always registered — the fix is discoverability: `agent` is optional,
  its schema names `general, explore`, the system prompt carries a delegation
  rule, and the `background` description states the configured default. The
  prompt now also says to spawn **before** exploration when the user asks.
- **Second real-model session** (`ses_e762e374…`): 1 build → 3 `general`
  (depth 1) → 6 `explore` (depth 2), all idle; 56 root assistant steps over
  15.8 min. It exposed three defects that are now fixed with tests:
  background results were `Steer` rows that the turn loop never promoted mid-turn
  (the root hit the 50-step limit before one of three child results arrived),
  `digest` truncated the child's final answer to 600 chars so the scribe refused
  to summarize, and a timeout left the child (and a pending permission ask)
  running. Fixed: step-boundary steer promotion, complete-final-answer digest,
  and timeout cancels the child and drops the ask.
- **Fake-provider binary harness** (ports 4097/4098, zero cost): spawn,
  nested depth 1–4 with policy rejection, observer tick → nudge delivery,
  SSE ordering/persist-before-publish, `kill -9` + restart durability — all
  pass. The observer-auth probe passes after the stored-credential fix.
- **Delivery modes**: `steer` is written to the transcript at admission and
  promoted at each step boundary; `queue` stays inbox-only until the running
  turn settles. A live pre-fix run showed a queued prompt visible to the
  running turn and answered twice; the admission split and step-boundary
  promotion are now covered by router-level tests.
- **Failure modes** (live, ports 4102/4103): background timeout, deleted child,
  child provider 500, permission-Ask child, exactly-once completion — all pass.
- **Observer repairs** (live, 4100/4101): compact_context, switch_model with
  escalation fallback, repost with pending guard, restart_subagent replay,
  goal_done persisted before a single `goal.updated` — all pass.
- **Siblings** (live, 4112/4113): roster, note wake, question/answer, SSE
  `subagent.peer.message`, `session_write`, disabled mode, deleted target — all
  pass.
- **Auth/forms/permissions** (4106): Basic auth + SSE 401/200, form
  create/reply/cancel/409/400, broker ask/allow/deny/evaluate — pass; `always`
  persistence was the one failure and is now implemented with tests.
- **Fork/revert/export/import/schedule/worktree/PTY** (4108/4109): 77/77 checks
  pass. Worktree refresh was importing the main working tree; fixed (excluded
  and self-healed).
- **MCP/LSP/plugin** (4110/4121): MCP connect/tools/resources/disconnect and
  LSP hover/diagnostics/reload pass; the plugin sidecar is not attached by the
  CLI (documented gap).
- **Manual compaction** (`POST /api/session/:sid/compact`) consumes its inbox
  row, writes a `compaction` message (`completed` or `failed`), and never
  retries a failed row. Compaction now publishes durable v1 lifecycle events
  after each commit (`session.compaction.started`, `session.compaction.ended`,
  `session.compaction.failed`); the ephemeral `session.compaction.delta` is
  still absent.
- **Background action**: `POST /api/session/:sid/background` now detaches a
  foreground subagent wait (the tool returns `status:"running"`, the parent turn
  continues, the child's final text arrives later as one synthetic message).
  Shell `background:true` remains unsupported (docs/12); the UI session page no
  longer throws `shellID` when a running tool has no metadata (`0004`).
- **UI**: model picker lists and selects the config-defined model again
  (`0003-model-release-unknown.patch`); context shows **1,024,000**; the
  10-node tree, settings tabs, goal chip and model switch match the API.
  Console noise is limited to the upstream goal RPC (404) and worktree refresh
  on non-git dirs.
- **Route matrix**: every documented route group answered truthfully; the
  remaining gaps 1–7 returned `501`/`503`/404 rather than faking success.
  Docs corrected for pending-only form lists, required `projectID` on
  `GET /api/worktree`, and OAuth/command connect returning `400` for unknown
  catalog methods.

## Remaining gaps (real, verified)

1. **Code Mode `execute`** — no JS sandbox or catalog; the tool is not
   registered.
2. **Image generation** — `/api/experimental/generate/image` (and
   `/generate/text`, `/generate/decisions`) answer `503`; no generator service
   is attached.
3. **Voice** — no STT/TTS routes or tools.
4. **Client-MCP tunnel** — `GET /api/experimental/client-mcp` is not registered;
   `POST /api/experimental/client-mcp/token` answers `503`.
5. **Plugin sidecar host/update** — the JSON-RPC sidecar exists in `oc-plugin`
   with its own tests, but the served CLI attaches only a bare
   `PluginRegistry::new()`; `plugin_host.command` is never started, so
   `GET /api/plugin` is empty and `/api/rpc/<namespace>` stays unavailable.
   `POST /api/plugin/update` answers `503` (no installer); GUI bundles are not
   loaded (`/api/plugin/gui` → 404).
6. **Full location reload** — `POST /api/location/reload` only rescans skills;
   the immutable `ConfigService` snapshot is not rebuilt, so config changes need
   a restart (`PATCH /api/experimental/config` sets
   `x-opencode-restart: required`).
7. **V1 import** — no upstream/V1 importer; `GET /api/experimental/migration/v1`
   answers `501`.
8. **Observer repair audit precedes apply** — the CLI repair channel is
   fire-and-forget: `RepairExecutor::apply` returns once the command is queued,
   so the engine writes a repair row and publishes `observer.repaired` even when
   the runtime rejects the repair (verified with a catalog-unknown
   `switch_model`; the model is correctly not persisted, only the audit/event
   overstate success). A reply channel on `RepairCommand` would close it.

Snapshot capture is intentionally bounded, not a gap: only pre-write snapshots
of files inside the session directory are kept, each file is capped at 5 MiB,
and unreadable/non-regular/oversized files are recorded as skipped (revert and
diff leave those paths alone).

Also truthful by design (not bugs): media blobs are not served
(`/api/experimental/media/:hash` → 404), `POST /api/websearch` is not
registered, websearch providers list is empty, usage limits are `null`, and
`DELETE /api/debug/location`/`POST /api/debug/heap-dump` answer `501`.

## Notes for the next integration owner

- `Runtime` owns execution; the server must not execute model calls inline.
- Event ordering: persist (store) → publish (bus) → return HTTP. Never publish
  a terminal event before commit.
- Durable route events go through `data::events::publish_durable`; ephemeral
  notifications use `EventEnvelope::now` + `AppState::publish`.
- `oc-store` bodies are wire `Value`; do not add a second entity layer unless
  the JSON shape diverges.
- Optional fields are omitted, never `null`. Unknown routes under `/api` return
  JSON 404.
- A crate-level gate is `cargo fmt -p X`, `cargo clippy -p X --all-targets -- -D
  warnings`, `cargo test -p X`; the workspace gate runs once per wave.

## Known deviations / gaps (smaller)

- **One server per database:** `Store::open` holds an exclusive `<db>.lock` for
  the process lifetime; a second server on the same file fails with a conflict
  instead of racing boot recovery. Multi-process service on one DB is not a
  supported topology.
- **Recovered children settle as failed:** a background child repaired on boot
  is delivered to its parent once as `<subagent state="failed">`; it is not
  auto-resumed (the process-local watcher died with the old process).
- **Compaction keeps the tail:** completed compaction rows carry `keepID`;
  context assembly replays the active turn (or `compaction.keep.tokens` budget)
  after the summary.
- **Data directory separation:** The server's default SQLite path uses `$ESCAPE_DATA_DIR` or `<data>/escape-agent/escape.db` (or `opencode-rust.db` when `$OPENCODE_DATA_DIR` is explicitly specified), preventing schema collisions with upstream OpenCode installations.
- The upstream goal GUI plugin calls `POST /api/rpc/opencode.goal/get`; the
  server has no `opencode.goal` RPC namespace, so that request is a JSON 404
  (the plugin swallows it). Goals are served by `/api/experimental/goal` and the
  patched goal chip uses that route.
- Observer `inspect` still does not generate a digest, and `retry_tool` /
  `cancel_tool` remain typed no-ops (no per-tool cancellation token). The other
  repair kinds now act: compact, switch_model (with escalation fallback),
  repost (last user prompt + pending guard), restart_subagent (re-enqueue).
- A subagent spawn does not publish `session.created`; the tree panel updates
  on remount/`staleTime` (30 s) rather than live.
- `max_children_per_session` is enforced by policy plus the in-process spawn
  budget; the store insert itself is not a cross-process transaction.
- A sibling message storm is a hard `SiblingBudget` error, not a queue.
  `request_reply` is stored but no reply correlation notifies the sender.
- `oc-types::SessionInfo.location` is `SessionLocation {directory, workspaceID?}`
  (exact wire) rather than `LocationInfo {directory, project, subpath?}`. Both
  are exported; the server uses the wire one.
- `ConfigPatch.shell` cannot express `null` (no-null rule); there is no clear
  path yet.
- `oc-tools` read media uses a base64 data URL in `content.uri` because the tool
  result has no byte channel; the runtime does not convert it to `media:` refs
  (and this server has no blob store anyway).
- `oc-tools` shell spills combined output to a temp file always; upstream only
  spills on truncation.
- Plugin JS sidecar is a bridge, not full Effect compatibility (docs/10 matrix).
- LSP auto-download is off by default (deliberate).
- No retention/maintenance sweep is implemented; rows accumulate (see
  `docs/03-storage.md`).
- `session.v1` import is rejected; only the v2 schema is read.

## Code Intelligence, Partitioned Subagents & LSP Precision

Verified workspace suite for this release wave: **1,155 passed, 0 failed, 3 ignored** (1,152 unit/integration + 3 doctests passed; 3 pre-existing doctests ignored; `oc-tools`: 100, `oc-runtime`: 274 + 1 doctest, `oc-config`: 42, `oc-subagents`: 22, `oc-server`: 254), expanding the earlier 12-operation `code_map` engine above to 18 operations plus atomic symbol editing:

- `symbol_edit` tool in `oc-tools` (`replace` | `insert_before` | `insert_after` | `preview`) via `ripwire --mcp` with pure-Rust AST-span fallback, BOM/CRLF preservation, and unified diffs in `metadata.files`; preview calls map to `read` permission and skip snapshot/mutation tracking, while edits map to `edit` permission (including `symbol_edit` → `edit` config normalization in `oc-config`).
- Zero-noise post-edit radar (`oc_tools::code_map::collect_post_edit_radar`) and `oc_runtime::EditDiagnosticsHook` (`PostEditDiagnostics`) run on non-preview code mutations (`write`, `edit`, `patch`, `symbol_edit`) and append `<lsp_diagnostics>` / `<post_edit_radar>` only when actionable findings exist; `verify_on_stop` nudges include top radar blockers via `build_verification_nudge_with_radar`.
- `code_map` expanded with `pack_task`, `lego`, `slice`, `cochange`, `doc_drift`, `flags`, `quality` (`delta` | `dead_code` | `clones` | `hotspots` | `deps` | `seams`), `var`, `flow`, `depth`, `partitions` (2..16), and `precision` (`fast` | `scip` | `lsp` | `hybrid`) plus `ripwire --mcp --scip=<path>` support and `SemanticPrecisionOracle` (`tokio::task_local!` via `with_semantic_oracle`).
- `subagent` tool supports atomic `partitions` (2..16) parallel fan-out via `SpawnBudget::try_spawn_batch` (injecting `<shared_core>` + `<partition>` slices and `<subagent-partition>` aggregation) as well as manual `partition_index`/`partition_count`.
- `oc-server` (`LspEditDiagnosticsHook`, `LspSemanticOracle`) and `oc-cli` (`bootstrap.rs`) wire `LspSupervisor` directly into `Runtime` for live post-edit diagnostics and `lsp`/`hybrid` `code_map` precision.

## Verification & Development Flow

1. Review architecture and specifications in `docs/` before making API changes.
2. Verify interfaces and update `docs/17-interfaces.md`.
3. Run workspace verification suite:
   ```sh
   cargo fmt --all --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   ```
4. Record behavior tests and update this verification log accordingly.
