# 20 — Engineering & Integration Guide

This guide documents the runtime architecture, verified component state, and developer workflows for the Escape Agent (`opencode-rust`) workspace.

Wire compatibility is defined by `../opencode/packages/protocol/openapi.json` and `../opencode/packages/client/src/promise/generated/types.ts`. The landed server is described in `docs/02-protocol.md`.

## Verified State

- Workspace gate: `cargo fmt --all --check` clean, `cargo clippy --workspace --all-targets -- -D warnings` clean, `cargo test --workspace` reports **803 tests / 80 suites / 0 ignored / 0 failed**.
- The binary passes comprehensive smoke tests (UI, SSE, session CRUD, provider-less truthful failure, shell process/capture) plus `oc-cli`'s binary-level E2E suite **5/5** (text turn, read-tool turn, restart durability, static UI, permission prompt).
- The codebase is modularized without changing public paths or wire envelopes (layout map in `docs/17-interfaces.md`). Compaction `keepID` is stored as top-level message data.
- Live smoke previously passed **29/29** (auth P0, SSE filter, forms, shell,
  schedule, UI, restart durability). The post-snapshot quick smoke also passed
  on the prior gate binary (info/location bare/session/diff empty/shell
  output/schedule create+delete/SSE first frame/UI title `OpenCode`, zero
  console errors, clean shutdown).
- Project registration and the directory picker work end to end: `GET
  /api/location` upserts the `LocationRef` project into the store, and
  `GET /api/fs/list` navigates absolute/relative paths (including parents and
  other drives) for the picker while reads/finds stay confined.
- Provider availability is real: `/api/provider` and `/api/model` list only
  providers that are configured in `providers`, backed by a stored credential,
  or reachable through a declared env var (disabled providers excluded);
  `/api/integration` lists every non-disabled catalog provider as connectable
  via `key` plus an `env` method; the runtime injects a stored credential key
  into the provider client (active credential first, then any matching one).
- Observer credential resolution was fixed: a declared-but-unset
  `provider.env` no longer disables the observer. Credentials are resolved per
  judge request from the stored credential table or provider env; a judge call
  with no usable credential fails open and publishes `observer.unavailable`.
- The server implements the routes listed in `docs/02-protocol.md`, including
  sessions/messages/forms/permissions, goal/skill/lane/schedule, MCP/LSP/plugin,
  shell manager + PTY, credential/integration and worktree. Anything missing
  answers a truthful `501`/`503`.
- The upstream web app builds into `web/dist` and is served by the Rust server.
  Settings tabs (memory/goals/observer/subagents/lanes) and session agent panels
  (subagent tree, peer drawer, observer repairs/budget, goal chip, lane pill) are
  in the built bundle.
- Real-provider E2E was exercised through the binary against an
  OpenAI-compatible provider (text turn, `read` tool turn, subagent turn) with
  the key supplied at runtime; no secret is in the repo.

## What exists

| Layer | State |
|---|---|
| `oc-types` | full wire model + typed event payloads |
| `oc-store` | SQLite WAL, sessions/messages/inbox/events, memory FTS, projects (registered from `LocationRef`), goals/observations/repairs, subagent tree, forms, schedules, snapshots, instruction entries, saved permissions, lane tickets, credentials, worktrees |
| `oc-config` | discovery/JSONC/V1 aliases, agents, permissions, skills, commands, AGENTS.md |
| `oc-llm` | embedded models.dev snapshot, catalog/resolve, OpenAI/Anthropic/Gemini streaming, retries |
| `oc-runtime` | session actors, turn loop, tools + permissions, compaction, cancellation/recovery, memory, observer strips, subagents, digest, goal/skill/lane tools, lane runner, stored-credential key injection |
| `oc-tools` | read/write/edit/patch/glob/grep/shell/todowrite/webfetch + registry merge |
| `oc-mcp` / `oc-lsp` | stdio+HTTP MCP with cached tool provider; LSP supervisor with `lsp`/`lsp_diagnostics` tools |
| `oc-plugin` | native registry + JSON-RPC stdio sidecar |
| `oc-memory` / `oc-observer` | scoped retrieval/tools, scribe library + CLI LLM scribe (digest and auto-scribe), strict observer + goal machine + repairs + persisted goal transitions |
| `oc-subagents` / `oc-lanes` / `oc-pty` | tree policy, lane scheduler, ConPTY |
| `oc-server` | HTTP/SSE/WS, auth, static, routes above; available-provider filtering, integration catalog, fs/list picker navigation |
| `oc-cli` | `serve` with store/config/catalog, runtime, memory + LLM scribe (`digest_model`), observer, MCP/LSP, PTY, plugins, lanes, schedule runner |
| UI | vendored upstream app source (`ui/vendor/opencode`, MIT, already contains `ui/patches/0001`–`0004`); `ui/build.sh`/`build.ps1` install frozen, build `@opencode/app`, replace `web/dist`, and run `ui/verify-dist.mjs` |

Sessions, messages, parts, inbox, events, projects, memory, goals, schedules,
snapshots, credentials, worktrees and lane tickets all live in one SQLite file.
Resolution: `--db` / `$OPENCODE_DB`, else `$OPENCODE_DATA_DIR/opencode.db`, else
`$XDG_DATA_HOME/opencode/opencode.db`, else `$HOME/.local/share/opencode/opencode.db`,
else `%TEMP%/opencode/opencode.db` (Windows without `HOME`). WAL sidecars are
`<db>-wal`/`<db>-shm`; the repo is untouched unless `--db` points into it. See
[03-storage.md](03-storage.md#where-sessions-are-stored).

## Remaining gaps (do these next, in order)

1. **Code Mode `execute`** — JS sandbox + catalog; not registered.
2. **Image generation** — `/api/experimental/generate/{text,image,decisions}`
   answer `503`; no generator service.
3. **Voice** — no STT/TTS.
4. **Client-MCP tunnel** — WS route absent; token route `503`.
5. **Plugin sidecar host/update** — the sidecar exists but the served CLI only
   attaches a bare registry; `plugin_host.command` is never started. Update
   route `503`; GUI bundles not loaded.
6. **Full location reload** — reload rescans skills only; config requires a
   restart.
7. **V1/upstream DB import** — importer absent; migration route `501`.

## Known pitfalls

- **Model ids may contain `/`** (e.g.
  `river/deepseek-ai/DeepSeek-V4.1-Flash-1017`). `oc-config` and `oc-subagents`
  split on the first `/`; do not reintroduce a "no slash in model id" check.
- **Tool trait names are `&str`, not `&'static str`.** MCP tools back their
  names with `String`; changing the signature back breaks dynamic tools.
  `ToolError` is a `thiserror` struct with `new`/`with_metadata`.
- **Message ids are globally unique rows, not per-session.** `session_message.id`
  is the primary key and `(session_id, seq)` is the ordering key; lookups pass
  both. Events map to messages by stripping `evt_`/adding `msg_`; never mint a
  second id.
- **No Lua.** Plugin hooks are native Rust traits or a JSON-RPC JS sidecar; there
  is no Lua interpreter and none should be added for parity.
- **Test isolation**: the environment may export
  `OPENCODE_CONFIG_CONTENT`; every config test must shadow it with `" "` (space,
  not empty). Use `tempfile`; do not leave `TMP` pointed at a project temp dir.
- **Runtime parent model**: session rows may have no `model`; the subagent tool
  falls back to the config default. Keep that fallback.
- **SSE/event ordering**: persist before publish; never append a durable event
  the runtime already persisted. Ephemeral events must not reserve a seq.
- **Wire envelopes**: the generated client does not strip `{data}`. Follow each
  OpenAPI schema — `/api/worktree` is a bare array/object, session create is
  `{data}`, location lists are `{location,data}`.
- **UI patch**: apply patches in name order to a pristine upstream checkout;
  `git apply --check -p1` must pass before building. Bun 1.4.4 is installed.
- **Schedule time zones are limited.** `when.tz` accepts `UTC` and numeric
  `±HH:MM` only; an IANA name (`Europe/Berlin`, `America/New_York`) is a 400,
  never silently UTC. Cron is five-field (`minute hour day month weekday`);
  `every` must be ≥ 1 minute; `at` must be in the future.
- **Web password sources are flag/env only.** `--password`,
  `OPENCODE_PASSWORD`, `OPENCODE_SERVER_PASSWORD`; explicit empty disables auth.
  `serve --service` reuses `service.json:password` (or generates one) and
  `opencode service get password` prints it. The static shell and
  `/auth/connect` stay public so the app's connect/login screen can load; every
  `/api` route is protected. Do not add a plaintext password key to
  `opencode.json`.
- **`/compress` is a server builtin alias of `/compact`.** It appears in
  `GET /api/command`, is accepted by `POST /command {"name":"compress"}`, and a
  raw prompt of exactly `/compress` (case-insensitive) admits a compaction row
  instead of a user message. A markdown `compress` command overrides the
  builtin through the normal command path.
- **The served directory is absolute.** `serve` applies
  `std::path::absolute`; default location resolution joins `.` (never the
  directory onto itself), which prevents `work/work`. Static directory paths
  (`/_assets`, `/icons`) are 404, not the "UI not built" 503.
- **Compaction reason is part of the wire shape.** Inbox-driven (manual)
  compaction writes `reason:"manual"`; inline auto compaction writes
  `"auto"`. Never hardcode `"auto"` in the shared compact path (a test pins
  both the completed/failed rows).
- **Compaction `keepID` is top-level message data.** The runtime writes it on
  the compaction row; fork/import must read and remap `keepID` at the top level
  (legacy `/metadata/keepID` is migrated and removed). Both routes have a
  regression test.
- **Saved permission rules use config wildcard matching.** `permission_saved`
  rows are matched with `oc_config::wildcard_match` (case-insensitive on
  Windows, `/`-normalized) and are consulted by both the runtime ask path and
  the direct `POST /api/session/:sid/permission` route. `reject` denies every
  pending ask of the session, not only the answered one.
- **Foreground subagent waits are cancellation-guarded.** `ToolContext.cancel`
  carries the execution's `CancellationToken`; the subagent tool spawns a
  detached guard that interrupts the child when the parent future is dropped.
  Do not remove the guard or pass `None` from the runtime.
- **Synthetic message ids are durable keys.** A second `POST /synthetic` with an
  id that already became a message is a 409, mirroring `/prompt`; the runtime's
  doorbell branch would otherwise consume the retry without executing it.
- **Snapshot capture is bounded.** Pre-write snapshots exist only for
  `write`/`edit`/`patch` on paths inside the session directory; per-file content
  is capped at 5 MiB and unreadable/non-regular/oversized files are recorded as
  `skipped` (revert/diff leave them alone). Snapshot ids are `snap_…`.
- **Do not commit secrets**: credentials live in the SQLite `credential` table
  (the integration's active row wins) or the provider's environment variables;
  never in repo config.

## Exact commands

```sh
cd opencode-rust
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p oc-cli -- serve --port 4096
# UI: http://127.0.0.1:4096/  (web/dist is present)

# UI rebuild (needs Bun + ../opencode):
./ui/build.sh          # or build.ps1 on PowerShell
```

For a custom OpenAI-compatible provider, put this in `.opencode/opencode.jsonc`
(or the global config), then export `LOCAL_API_KEY` — never put the secret in
the file:

```jsonc
{
  "model": "local/llama-3.1-8b",
  "providers": {
    "local": {
      "name": "Local OpenAI-compatible",
      "package": "@opencode/ai/providers/openai-compatible",
      "env": ["LOCAL_API_KEY"],
      "settings": { "baseURL": "http://127.0.0.1:11434/v1" },
      "models": {
        "llama-3.1-8b": {
          "name": "Llama 3.1 8B",
          "settings": { "temperature": 0.2 },
          "capabilities": {
            "tools": true,
            "input": ["text"],
            "output": ["text"],
            "reasoning": {
              "supported": true,
              "effort": ["low", "medium", "high", "max"]
            }
          }
        }
      }
    }
  }
}
```

The `capabilities.reasoning.effort` list creates model variants
`low|medium|high|max`; they appear in the composer's **Choose model variant**
menu and each sends `reasoning_effort` through the OpenAI protocol. Omit it (or
set `supported: false`) to hide the selector. `tools`/`input`/`output` describe
capabilities to the UI and runtime.

The provider becomes available because it is configured (so it appears in
`/api/provider`, `/api/model` and `/api/integration`); `baseURL`, `headers`,
`body` and per-model `settings` are merged into the request. A key entered
through `/api/integration/<id>/connect/key` (or `POST /api/credential`) is
picked up by the runtime without a restart; the `env` list is only an
alternative source. `model` references are `<provider>/<modelID>`.

For a live smoke: `curl` `/api/info`, create a session, `POST /prompt`, then read
`GET /api/session/:id/message` and the SSE `/api/event` stream.
