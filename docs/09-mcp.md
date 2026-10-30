# 09 — MCP

Full client support: stdio, streamable HTTP, legacy HTTP+SSE (compat), and the
inverted WebSocket "attached" transport. OAuth for remote servers. Tools,
prompts and resources are exposed to the agent exactly like upstream.

## Transports

| Config `type` | Transport | Framing | Notes |
|---|---|---|---|
| `local` | stdio child | newline-delimited JSON-RPC | env/cwd/command; stderr tail 1 KiB |
| `remote` | streamable HTTP | POST JSON + SSE responses | `Mcp-Session-Id`, resumable via `Last-Event-ID` |
| `remote` (legacy) | HTTP+SSE | GET event stream + POST endpoint | only when `protocol:"legacy"` |
| attached | WebSocket | one JSON-RPC message per text frame | browser hosts the server; opencode is the client |
| client-mcp | WebSocket tunnel | same as attached | desktop/plugin path |

Protocol negotiation: `protocol: "legacy" | "auto" | "2026-07-28"`. `auto`
probes the newest revision, falls back to the legacy `initialize` handshake and
reports the supported set on failure. Default = legacy initialize up to
`2025-11-25` (upstream behavior).

Handshake: `clientInfo {name:"opencode", version:"<our version>"}`, capabilities
`roots:{}`, `elicitation:{form:{},url:{}}` when a handler exists,
`listChanged:{tools,prompts,resources}` (no auto-refresh; refresh on demand).
`roots/list` returns one `file:` root = the location directory.

## Config

```jsonc
{
  "mcp": {
    "timeout": {"startup": 30, "catalog": 30, "execution": 43200},
    "servers": {
      "github": {
        "type": "remote",
        "url": "https://mcp.example.com/mcp",
        "headers": {"Authorization": "Bearer {env:GITHUB_TOKEN}"},
        "oauth": {"client_id": "…", "scope": "repo"},
        "protocol": "auto",
        "codemode": true,
        "disabled": false
      },
      "fs": {
        "type": "local",
        "command": ["npx", "-y", "@modelcontextprotocol/server-filesystem", "."],
        "cwd": ".",
        "environment": {"NODE_OPTIONS": "--max-old-space-size=512"},
        "timeout": {"startup": 60, "execution": 600}
      }
    }
  }
}
```

`oauth: false` disables OAuth for a server. `codemode` controls whether tools
are hidden behind the `execute` catalog (upstream default true when Code Mode
is enabled); with Code Mode off, tools are exposed directly.

## Lifecycle

- Connect happens in the background at location activation; a failed server
  never blocks the location or other servers.
- Per-URL mutex serializes concurrent connects to the same endpoint.
- Statuses: `connected | needs_auth | failed | disabled`; emitted as
  `mcp.status.changed {server}`.
- `onClose` → `failed` with reason, tools dropped, no automatic reconnect loop;
  reconnect on next explicit use or `POST /connect`.
- Legacy session expiry (HTTP 404, or 400 containing "Server not initialized",
  with a session id) → reconnect once and retry foreground calls.
- Transient retry: 408/429/5xx (except 501) or network errors, exponential from
  250 ms, 2 extra attempts, bounded by the startup deadline.
- Shutdown: close in parallel; stdio `SIGTERM`, then `SIGKILL` after 2 s;
  wait ≤ 1 s for exit.

Timeouts: startup 30 s, catalog 30 s, execution 12 h (10 min for attached),
close 1 s. 16 MiB max stdout frame, 64-frame outgoing queue.

## Tool exposure

- Effective name: `<server_sanitized>_<tool_sanitized>` (`[^A-Za-z0-9_-]` → `_`),
  namespace = server name. Name collisions across servers are refused at
  registration with a `PluginError` listing both.
- Input schema = the server's JSON Schema, **patched** with the required
  `expectation` property before it is shown to the model; stripped on dispatch.
- Permission action = the full effective tool name; default from the agent
  ruleset (`*:* allow` by default).
- Result mapping:
  - `text` → text content;
  - `image`/`audio` → media content (`mimeType`, base64);
  - `resource_link` → URI text;
  - `resource` with text → text, blob → media, else URI text;
  - `structuredContent` → `metadata.structured` and JSON text when no output
    schema;
  - `isError` → `Tool.Error` with joined text.
- Arguments pass through unchanged except `expectation`.
- `tools/list` pagination is followed to the end; an invalid tool is dropped
  with a warning, the rest are kept (upstream behavior).

Resources and prompts:

- `GET /api/mcp/resource` → catalog `{server, resources[], prompts[]}`.
- `mcp_resource` tool reads one URI (≤ 5 MiB).
- MCP prompts are exposed as commands (`/<server>:<prompt>`) and through
  `POST /api/session/:id/command`.

## OAuth

Each remote server with `oauth !== false` registers an integration:

- id = `mcp_<sha1(name + "\0" + url)[0:16]>`, metadata `{source:"mcp"}`,
  method `{type:"oauth", label:<name>}`.
- Credentials live in the `credential` table (0600 DB); refresh tokens are
  updated in place; `credential.updated` is emitted.
- Flow: `POST /api/integration/:id/connect/oauth` → attempt, then
  `GET /api/integration/:id/connect/oauth/:attemptID` until `complete`, with
  `POST …/complete {code?}` for the manual code path and `DELETE` to cancel.
- `UnauthorizedError` at connect → `needs_auth`; anything else → `failed`.
- Token refresh happens before execution when the credential is within 60 s of
  expiry; a failed refresh becomes `needs_auth`, never a silent retry storm.

## HTTP routes

Existing (upstream):

| Route | Purpose |
|---|---|
| `GET /api/mcp` | `Mcp.Server[]` status/config per location |
| `GET /api/mcp/resource` | resource/prompt catalog |
| `PUT/DELETE /api/experimental/mcp/:server` | add/remove server config |
| `POST /api/experimental/mcp/:server/{connect,disconnect}` | manual lifecycle |
| `POST /api/experimental/client-mcp/token` | tunnel token |
| `GET /api/experimental/client-mcp` | WebSocket tunnel |

Additive: `GET /api/experimental/mcp/:server/tools` (debug view with effective
names and schema hashes), `GET /api/experimental/mcp/:server/health`.

## Rust implementation

- Prefer `rmcp` for protocol types and the stdio/streamable-HTTP clients. If a
  required revision or transport is missing, fall back to a hand-rolled
  JSON-RPC layer (`serde_json::Value`, `Content-Length` for stdio framing,
  newline framing for MCP stdio per spec).
- `tokio::process` with `kill_on_drop`; on Windows a Job Object so grandchildren
  die with the server; on Unix a process group.
- No `npx` wrapper unless configured: spawn the exact command.
- Do not auto-install MCP servers.
- Cache tool lists per server with a schema hash; a `tools/list_changed`
  notification refreshes in the background and emits `mcp.status.changed`.
- Bound total MCP child processes (default 8) and connect concurrency (4).

## Failure matrix

| Failure | Model sees | UI sees |
|---|---|---|
| Server failed at boot | tools absent | `failed` + reason |
| Needs auth | tools absent | `needs_auth` + connect CTA |
| Tool call timeout | `Tool.Error` "MCP tool timed out after Ns" | tool failed |
| Server crash mid-call | `Tool.Error` "MCP server exited" | status `failed`, tools dropped |
| Invalid tool schema | tool dropped with warning | status unchanged |
| OAuth refresh failure | `Tool.Error` "MCP authorization expired" | `needs_auth` |
