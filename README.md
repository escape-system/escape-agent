# opencode-rust

Rust server for the opencode web client, CLI and TUI.

The design is documented first and code follows it:

- Start at [`docs/README.md`](docs/README.md).
- Architecture and crate graph: [`docs/01-architecture.md`](docs/01-architecture.md).
- HTTP/SSE contract: [`docs/02-protocol.md`](docs/02-protocol.md).
- Roadmap: [`docs/16-roadmap.md`](docs/16-roadmap.md).

## Status

Working end-to-end chat: fake/real OpenAI-compatible provider → runtime → tool
loop → subagents → durable transcript → SSE. The full route list, parity gaps
and the new-session brief are in [`docs/18-status.md`](docs/18-status.md) and
[`docs/20-handoff.md`](docs/20-handoff.md).

Implemented crates: `oc-types`, `oc-store`, `oc-config`, `oc-llm`, `oc-runtime`,
`oc-tools`, `oc-mcp`, `oc-lsp`, `oc-plugin`, `oc-memory`, `oc-observer`,
`oc-subagents`, plus the server/CLI. The web UI builds with Bun and is served
by the Rust binary.

Still in progress: agentic wiring (memory/observer/subagents/lanes), settings
UI, MCP/LSP/plugin HTTP wiring, PTY, Code Mode, experimental routes and the
performance/parity pass.

## Build and run

```sh
cargo build
cargo run -p oc-cli -- serve --port 4096
curl -s http://127.0.0.1:4096/api/info
curl -N http://127.0.0.1:4096/api/event
```

With a password:

```sh
cargo run -p oc-cli -- serve --password secret
curl -s -u opencode:secret http://127.0.0.1:4096/api/info
```

## Test

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

## Web UI

The upstream SolidJS app is built from a pinned commit with a patch set. The
fork is branded **Escape Agent** through the theme, `src/escape/GlobalStyles.jsx`
and the brand assets; upstream layout and routes are left intact.

```sh
ui/build.sh          # or ui/build.ps1
```

Output goes to `web/dist`. The server serves it from disk when present (phase 1
adds in-binary embedding); until then `/` returns 503 with a build instruction
and API routes are unaffected.

Live end-user checks (run against a running server, no fixture web server):

```sh
cd ui/vendor/opencode/packages/app
bun run test:e2e:escape
```

Set `ESCAPE_BASE_URL`, `ESCAPE_AUTH_URL`/`ESCAPE_AUTH_PASSWORD`, and
`ESCAPE_CHROMIUM` when the machine uses a different server or Playwright
browser cache. See [`docs/13-ui.md`](docs/13-ui.md).
