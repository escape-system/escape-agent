# 18 — Implementation status

Updated when the gate is green. This is the handoff document if the current
agent's context ends.

## If you are a new agent, start here

1. Read `AGENTS.md`, `docs/17-interfaces.md`, then this file.
2. Inspect `proc://` (running jobs/services) and `agent://<id>` (handoffs). If a
   wave is running, do not edit its crates; wait or take an unowned crate.
3. Once no agent is mid-write, run the workspace gate and fix any break before
   adding features.
4. Pick the first item under **Remaining gaps**; freeze its public API in
   `docs/17-interfaces.md` before fanning out subagents.
5. Update this file (state, tests, gaps) after every wave. A crate is not
   "done" until its gate is green and this table says so.

Copy-paste resume prompt for a fresh agent:

```
Read AGENTS.md, docs/README.md, docs/17-interfaces.md and docs/18-status.md.
Run proc:// to see if a wave is still running. Do not edit crates owned by a
running agent. Verify the workspace gate, then continue from the first item in
"Remaining gaps" in docs/18-status.md. Keep docs/17 (interfaces) and docs/18
(status) updated after each crate lands. No stubs; wire-compatible JSON.
```

## Snapshot

**Gate is green after the 2026-10-30 modularization wave.** `cargo fmt --all
--check` and `cargo clippy --workspace --all-targets -- -D warnings` are clean;
`cargo test --workspace` (run with `CARGO_TARGET_DIR=target/verify` because the
live 4096 process holds `target/debug/opencode.exe`) reports **803 tests / 80
suites / 0 ignored / 0 failed**. A fresh `target/verify/debug/opencode.exe`
(53,785,088 bytes) was built and live-smoked; the 4096 server still runs the
previous binary until restarted.

The wave split every source file that exceeded 1,000 lines, and every
integration-test file that exceeded 1,000 lines, into a facade plus focused
submodules without changing public paths, serde shapes, routes or error payloads
(map in [17](17-interfaces.md#module-layout-oversized-file-split)). The largest
split source module is 500 lines (`oc-runtime/src/turn/stream.rs`), down from
`turn.rs` 2,722; the largest remaining source file is 964 lines
(`oc-observer/src/engine.rs`), a candidate for the next wave.
`oc-server` `session_ext.rs` 1,945 → 15 files, `shells.rs` 1,754 → 8,
`observer_host.rs` 1,287 → 7, `permission.rs` 1,095 → 6, worktree/vcs →
5/6 + shared `data/git.rs`; `oc-llm` `catalog.rs` 2,426 → 15, `oc-config`
`normalize.rs` 1,700 → 16 (+`jsonc`/`merge`), `oc-store` `schedule.rs` 1,262 →
8, `oc-plugin` sidecar → 8, `oc-cli` main → 9, MCP/LSP clients → 8/12. Six
integration-test files over 1,000 lines (`oc-runtime` runtime/steer_live,
`oc-store` store_ext, `oc-server` session_ext/runtime/data) were split into
topic modules with identical test counts.

Test expansion: **+32 new behavioral tests total** (11 landed before the
intermediate gate), and **7 non-behavioral getter/prose tests removed**, for a
net **778 → 803**. Coverage added: runtime steer FIFO/recovery/compaction
reasons, server form validation/export sanitize/import rejection, shell UTF-8
cursor + session env, worktree confinement, cron/weekday/duration/ends, config
permission precedence, MCP/LSP framing. A real defect was fixed: compaction
`keepID` lives at the top level of message data, but fork/import read
`/metadata/keepID`; both now remap top-level (legacy metadata migrates) with a
failing-before/passing-after regression test.

Live evidence this wave: a real task on 4096 (`read` tool, 17 durable frames,
live SSE deltas + busy/idle, clean DELETE) passed; the new binary on an isolated
port/database passed a 16-check smoke (UI, SSE, session CRUD, provider-less
truthful failure, shell process/capture); and `oc-cli`'s binary-level E2E suite
passed **5/5** (text turn, read-tool turn, restart durability, static UI,
permission ask) against the new binary. The stability fixes and the earlier
feature/fix-wave matrices below remain landed.

## Verified

| Area | State |
|---|---|
| Workspace | 17 crates, edition 2024, `rust-version 1.90`; fmt/clippy clean, 803 tests / 80 suites / 0 ignored / 0 failed, `oc-cli` builds |
| `oc-types` | full wire model (session/message/inbox/permission/agent/file/config/location/info/error) + typed event payloads (`SessionViewed` included; fork/form/instruction/skill payloads are typed locally in the runtime/server) |
| `oc-store` | SQLite WAL, writer thread + 4-connection read pool, sessions/messages/inbox/events, dense-seq projection, memory FTS, goals/observations/repairs, subagent tree, forms, schedules, snapshots, instruction entries, saved permissions, lane tickets, credentials, worktrees, projects; migrations `0001`–`0010`; `register_project` upserts the `LocationRef` project on `GET /api/location` |
| `oc-config` | discovery/JSONC/V1 aliases, agents, permissions, skills, commands, AGENTS.md, rescan |
| `oc-llm` | embedded models.dev snapshot, catalog/resolve/overlay, OpenAI/Anthropic/Gemini streaming, retries |
| `oc-runtime` | session actors, turn loop, tools + permissions, auto/manual compaction (inbox `compaction` rows are consumed; manual rows write `reason:"manual"` and a failed inbox delete stops the actor instead of re-dispatching), mid-turn `steer` promotion at step boundaries (repost waits for idle; `switch_model` rejects a catalog-unknown target), cancellation/recovery, memory, observer strips, subagents (nested spawn for `general`, digest scoped to the caller's subtree with the child's full final answer and raw-answer cache honoring each caller's token budget, bounded watcher that cancels on timeout and delivers failed/timeout exactly once, parent cancellation interrupts a foreground child through `ToolContext.cancel`), `ToolProvider`, `GoalTools`/`SkillTools`/`LaneTools`, `ProcessLaneRunner`, lane events, pre-write snapshots, stored-credential key injection (active first, then any) |
| `oc-tools` | `read`, `write`, `edit`, `patch`, `glob`, `grep`, `shell`, `todowrite`, `webfetch`; registry merge, `&str` names; `ToolContext.cancel` (`CancellationToken`) lets long-running tools stop detached helpers when the execution is interrupted |
| `oc-observer` | registry, terse input, strict verdicts, budgets, goal state machine, repairs; judge credentials resolved per request from the credential store or provider env (declared-but-unset env no longer disables it), failures publish `observer.unavailable`; goal verdicts persist to the store before `goal.updated`; switch_model falls back to `observer.escalation_model`, repost replays the last user prompt (with a pending-input guard), restart_subagent re-enqueues the original child prompt |
| `oc-subagents` | session-tree policy, spawn limits, sibling/coordinator scope, write policy, model chain, budgets |
| `oc-lanes` | cargo/node/test classifier, priority lanes, semaphores, cancel/reorder/wait, max queue |
| `oc-memory` | scoped retrieval/render/budget, secret detector, tools, scribe library; the CLI attaches an LLM `ScribeModel` (`subagents.digest_model` → `observer.digest_model` → `observer.model`), so `digest` and turn-end auto-scribe work, gated by `memory.auto_scribe` |
| `oc-mcp` | stdio + streamable HTTP, handshake/tools/resources/prompts, OAuth PKCE helpers; manager cache + `McpToolProvider` |
| `oc-lsp` | supervisor, stdio framing, sync, push/pull diagnostics, tool ops (`lsp`, `lsp_diagnostics`), eviction/shutdown, `LspToolProvider` |
| `oc-plugin` | native registry/domains/hooks, JSON-RPC stdio sidecar, inventory/RPC |
| `oc-pty` | in-process ConPTY/PTY, 1 MiB ring, rows/snapshot/replay |
| `oc-server` | HTTP/SSE/WS, auth/pairing (password from `--password`/`OPENCODE_PASSWORD`/`OPENCODE_SERVER_PASSWORD`; explicit empty disables; Basic/session token/`auth_token`/same-origin cookie; static shell public so the connect screen loads), static app, sessions/messages/inbox/log, forms, fork/revert/context/diff/view/environment, export/import/stats/instructions, permissions (request + saved; `always` persists to `permission_saved`, auto-resolves covered asks, `reject` drains every pending ask of the session, saved rules use wildcard matching and answer the admission route too), builtin `compress` command + raw `/compress` prompt alias, model/provider (only available providers), config, fs (list can navigate outside the location for the picker; reads/finds stay confined), vcs, project, shell manager, persistent PTY, MCP/LSP/plugin/RPC, lanes, observer, goal, memory, subagent, credential/integration (catalog providers connectable via key+env), worktree (refresh takes the main tree from git's first porcelain record, excludes the project's own checkout and the main tree, self-heals stale rows), duplicate synthetic message ids are 409, default location resolution does not double a relative server directory, static directory paths are 404 not 503 |
| `oc-cli` | `serve` with store/config/catalog, password resolution (flag/env alias, empty disables, service reuses registration or generates), `service get <url|password|pid|id|version>`, runtime + LLM scribe attach (model chain normalizes null/empty `subagents.digest_model` so it still falls through to `observer.digest_model`/`observer.model`), memory, observer host, MCP/LSP, PTY, plugins, lanes, service registration |
| `oc-testkit` | fake provider/process harness + binary-level scenarios |
| UI | vendored SolidJS source (`ui/vendor/opencode`, MIT) built into `web/dist`; **Escape Agent brand layer** (dark default, gold/charcoal `oc-2` theme, glass panels/composer/tabs, centered home hero, six-letter wordmark + matching pin grid, branded connect/about, favicon/PWA/brand assets); settings tabs (memory, goals, observer, subagents, lanes) and session agent panels (subagent tree, peer drawer, observer repairs/budget, goal chip, lane pill); models with `time.released = 0` stay pickable (`0003`); missing tool metadata no longer crashes the session page (`0004`); `/compress` appears in the slash menu from `GET /api/command`; connect/login screen appears on a 401 and logs in with the configured password; live Playwright readiness suite in `packages/app/e2e/escape-agent` (**6/6** on 4096 + password 4097) |

### Route groups landed

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

### Fix-wave re-verification (binary 14:02:29, gate 767/78)

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
- **Data-directory collision with upstream opencode:** the Rust server's default
  SQLite path is the same `$XDG_DATA_HOME/opencode/opencode.db` the upstream JS
  server uses, and the two schemas are incompatible (the Rust migration table
  has a `checksum` column; the JS one does not). Starting without `--db` against
  an upstream database fails at open time. Use `--db <path>` (this workspace
  runs `.test-server/opencode.db`) or a dedicated data directory.
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

## Resume instructions

1. Read `AGENTS.md`, `docs/17-interfaces.md`, this file.
2. Check `proc://` and `agent://` for any still-running wave.
3. Run `cargo test --workspace` (only when no agent is mid-write).
4. Pick the next gap; freeze its interface in `docs/17` first.
5. Implement, test through the public surface, update this file.
