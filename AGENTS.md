# AGENTS.md — opencode-rust

Read `docs/README.md` and `docs/17-interfaces.md` before touching code. The
design is documented first; code follows the docs.

## Invariants

- Wire compatibility is defined by `../opencode/packages/client/src/promise/generated/types.ts`
  and `../opencode/packages/protocol/openapi.json`. When prose disagrees, the
  generated types win and the doc is fixed.
- Optional JSON fields are omitted, never `null`. Timestamps are epoch ms. IDs
  are prefixed opaque strings (`ses_`, `msg_`, `evt_`).
- `models.chain` / `models.chains.<role>` are a routing **preference order**,
  never a visibility filter. Chat-picker visibility is
  `!models.hidden[ref] && providerConnected`; the chain and `models.favorites`
  only reorder rows. `models.notes[ref]` is the only editable note.
- No stubs, `todo!()`, fake success, or placeholder behavior. A missing feature
  is reported, not faked.
- `unsafe_code` is forbidden workspace-wide. If FFI is ever required, it gets a
  dedicated crate and a safety proof.
- Every provider may hold several credentials; rotation happens per key on the
  same model and never surfaces as a model fallback. Provider key material never
  enters the config document or a log line. The primary-key pointer is durable
  (`credential.active`); per-key cooldowns are in-memory and reset on restart.
- Every durable boundary must survive kill -9; terminal events are published
  only after commit.
- Performance budgets in `docs/15-performance.md` are release gates.

## Workspace

| Crate | Phase | State |
|---|---|---|
| `oc-types` | 1/2 | wire model, typed event payloads |
| `oc-store` | 1 | SQLite schema/repos/event log/seq |
| `oc-config` | 1 | discovery/JSONC/merge/permissions/agents |
| `oc-llm` | 2 | catalog + OpenAI/Anthropic/Gemini streaming |
| `oc-antigravity` | 3 | Antigravity OAuth, multi-account pool, Cloud Code gateway, transforms |
| `oc-tools` | 2 | read/write/edit/patch/glob/grep/shell |
| `oc-mcp` | 3 | stdio + streamable HTTP client, tools/resources |
| `oc-lsp` | 3 | supervisor, diagnostics, tool operations |
| `oc-plugin` | 3 | native host + JS sidecar bridge |
| `oc-runtime` | 2 | session actors, turn loop, prompt assembly |
| `oc-observer` | 4 | expectations, observer, goals, repair |
| `oc-memory` | 4 | SQLite+FTS memory, scribe |
| `oc-subagents` | 4 | session-tree policy: spawn limits, sibling messages, model resolution |
| `oc-lanes` | 4 | build-lane classifier, priority queues, semaphore concurrency |
| `oc-server` | 1 | axum routes, SSE, auth, static |
| `oc-cli` | 1 | `opencode serve`, service registration |
| `oc-pty` | 3 | upstream `process-daemon` port |
| `oc-testkit` | 1 | fake SSE provider, server harness, HTTP/SSE helpers, `oc-bench` |

## Agents and model policy

Builtin agents (canonical ids, in canonical order): `engineer` (primary, the
DEFAULT — implements changes across crates, one owner per crate per wave),
`architect` (primary — docs-first design, writes design notes under `docs/`, no
code edits), `review` (primary — read-only diff review plus running the build
gates), `general` (subagent, the DEFAULT subagent, full tools), `explore`
(subagent — read-only search/navigation), `ui` (subagent — owns `ui/**` and
`web/**`), `docs` (subagent — owns `docs/**`, `README.md`, `AGENTS.md`), plus the
hidden system agents `compaction`, `title`, `summary`.

Legacy ids are aliases, normalized at config load and wherever a stored or
requested agent id is resolved (session rows, spawn targets, observer):
`build` → `engineer`, `plan` → `architect`, `reviewer` → `review`. `steward` is a
project template, never a builtin.

Model policy: `models.chain` / `models.chains.<agent>` and `models.fallback` are kept in configuration and `PUT /api/config/models` for compatibility with the Models control-plane UI, while **automatic model fallback is disabled**: turns and compaction execute only their resolved model (same-model multi-key rotation via `KeyPool` stays active), and subagent spawns never advance across `models.chain` or silently override an explicit `model:` argument. Chat-picker visibility is `!models.hidden[ref] && providerConnected`; the chain and `models.favorites` only reorder picker rows. `PUT /api/config/models` applies to the next spawn/turn without a restart; only an external edit of the config file needs one.

## Build gates (run from `opencode-rust/`)

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

During parallel work, scope to your crate:

```sh
cargo fmt -p <crate>
cargo clippy -p <crate> --all-targets -- -D warnings
cargo test -p <crate>
```

Never run workspace-wide commands while another agent is mid-write; wait for
the wave to finish or ask the coordinator.

## Release

Pushing a `vX.Y.Z` tag publishes a GitHub Release automatically; there is no
manual asset upload. The tag must equal `[workspace.package] version` in
`Cargo.toml` (minus the leading `v`), so bump the workspace version, refresh
`Cargo.lock` (`cargo metadata --format-version 1 > /dev/null`), and commit both
before tagging. `.github/workflows/release.yml` runs fmt/clippy/test, builds the
UI plus the `oc-cli` binary, and ships them as one archive with `web/dist` next
to the binary (exe-relative discovery). `workflow_dispatch` re-publishes an
existing tag. See `docs/23-release.md`.

## Test policy

- Tests are offline and deterministic: temp dirs, temp SQLite, fake HTTP/LSP/MCP
  servers, no API keys.
- Test behavior and wire shapes, not source text or getters.
- A bug fix gets a regression test that fails before and passes after.
- Scenario tests go through HTTP (`oc-server/tests/http.rs`), not internal
  functions, once the route exists.

## Coordination

- One owner per crate per wave; never edit another owner's crate.
- Public API changes are announced in `docs/17-interfaces.md` and in the
  handoff. Downstream crates must not be broken silently.
- Shared file edits (workspace `Cargo.toml`, `oc-server/src/lib.rs`) belong to
  the integration owner only.
- Use the standalone `lane`/big-brother protocol in
  `.opencode/skill/rust-build-test/SKILL.md` when several agents share a target
  directory.
