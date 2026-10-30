# AGENTS.md — opencode-rust

Read `docs/README.md` and `docs/17-interfaces.md` before touching code. The
design is documented first; code follows the docs.

## Invariants

- Wire compatibility is defined by `../opencode/packages/client/src/promise/generated/types.ts`
  and `../opencode/packages/protocol/openapi.json`. When prose disagrees, the
  generated types win and the doc is fixed.
- Optional JSON fields are omitted, never `null`. Timestamps are epoch ms. IDs
  are prefixed opaque strings (`ses_`, `msg_`, `evt_`).
- No stubs, `todo!()`, fake success, or placeholder behavior. A missing feature
  is reported, not faked.
- `unsafe_code` is forbidden workspace-wide. If FFI is ever required, it gets a
  dedicated crate and a safety proof.
- Every durable boundary must survive kill -9; terminal events are published
  only after commit.
- Performance budgets in `docs/15-performance.md` are release gates.

## Workspace

| Crate | Phase | State |
|---|---|---|
| `oc-types` | 1/2 | wire model, typed event payloads, 35 tests |
| `oc-store` | 1 | SQLite schema/repos/event log/seq |
| `oc-config` | 1 | discovery/JSONC/merge/permissions/agents |
| `oc-llm` | 2 | catalog + OpenAI/Anthropic/Gemini streaming |
| `oc-tools` | 2 | read/write/edit/patch/glob/grep/shell |
| `oc-mcp` | 3 | stdio + streamable HTTP client, tools/resources |
| `oc-lsp` | 3 | supervisor, diagnostics, tool operations |
| `oc-plugin` | 3 | native host + JS sidecar bridge |
| `oc-runtime` | 2 | session actors, turn loop, prompt assembly |
| `oc-observer` | 4 | expectations, observer, goals, repair |
| `oc-memory` | 4 | SQLite+FTS memory, scribe |
| `oc-server` | 1 | axum routes, SSE, auth, static |
| `oc-cli` | 1 | `opencode serve`, service registration |
| `oc-pty` | 3 | upstream `process-daemon` port |
| `oc-testkit` | 1 | fixtures, harness, benches |

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
