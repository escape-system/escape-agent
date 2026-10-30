# 10 — Plugins

Upstream v2 plugins are in-process Effect/TypeScript modules: `Plugin.define({id,
effect(ctx)})`, with live service objects, mutable transforms and hooks. That API
cannot be reproduced in-process in Rust. We ship two plugin classes instead:

1. **Native plugins** — Rust traits, compiled into the binary (built-ins and
   first-party features). Zero overhead, no dynamic loading.
2. **JS plugins** — an optional Bun sidecar running the upstream loader and
   plugin code, bridged over JSON-RPC. Compatibility is limited to the
   serializable subset documented below; the rest fails closed with a clear
   inventory error.

The sidecar starts only if `plugins` is non-empty or a plugin directory exists.
No plugins configured ⇒ 0 MB, 0 processes.

## 1. Native plugin surface

```rust
pub trait Plugin: Send + Sync {
    fn id(&self) -> &str;
    fn transform(&self, domain: Domain, doc: &mut JsonValue) {}   // agents, commands, skills, mcp, providers, models, tools, references, vcs, websearch, integrations
    fn hook(&self, hook: Hook, event: &mut HookEvent) -> Result<(), PluginError> { Ok(()) }
    fn rpc(&self) -> Option<&dyn RpcNamespace> { None }
}
```

- Built-ins are registered first (`pre`) and last (`post`); config plugins sit
  between, matching upstream activation order: builtin pre → sdk → instance →
  loaded packages → builtin post.
- Hooks are sequential in registration order; later hooks see earlier mutations.
- Only `tool.execute.before` may fail and abort the call. Everything else is
  infallible; a native plugin panic is caught and disables that plugin.
- Transformers operate on the same JSON documents the config layer uses, so
  their effect is observable in `GET /api/config` and reproducible in tests.

## 2. JS sidecar

### Process model

- One sidecar per server process (not per location), started lazily on the
  first request that needs plugin inventory.
- Transport: the sidecar's stdin/stdout, newline-delimited JSON-RPC 2.0
  (one child per server process; no socket). Frames ≤ 8 MiB.
- Runtime command: `plugin_host.command` from config (a string, or an array
  whose first element is the command and the remainder arguments); otherwise
  `bun`, then `node` from PATH.
- The sidecar runs Bun (preferred; upstream requires Bun APIs) or Node with a
  compatibility shim. If neither exists, JS plugins are marked `failed`
  (`reason:"runtime unavailable"`); the server keeps running.
- RAM budget: 90 MB RSS, 5 s startup timeout, restart on crash with backoff
  (1 s, 5 s, 30 s; max 3 per hour). After that, all JS plugins are `failed`
  until manual reload. `oc-plugin` enforces the startup/request deadlines,
  detects the crash and kills the tree; RSS accounting and the restart
  supervisor live in `oc-server`.
- Shutdown: `plugin.shutdown` RPC, 2 s grace, then kill the process tree.

### RPC surface

```
host → sidecar:  load {targets[]}, reload {ids[]}, hook {hook, event}, transform {domain, doc},
                 rpc {namespace, method, input}, dispose {id}
sidecar → host:  register {id, features, hooks[], transforms[]}, update {id, state},
                 log {level, message}, callback {requestId, method, args}
```

- `hook` is a request; the sidecar may mutate the event and return it, or return
  an error for `tool.execute.before`.
- `callback` lets a plugin call host services (session read, permission,
  storage, event subscribe) while a hook is running. The host correlates by
  `requestId` and enforces a 5 s deadline.
- No live objects cross the bridge. Types are projections with explicit
  schemas; unknown fields are preserved as `JsonValue` and passed through.

### Compatibility matrix

| Upstream hook/domain | Bridge | Notes |
|---|---|---|
| `tool.execute.before` | full | input JSON, may fail |
| `tool.execute.after` | full | result content/metadata |
| `session.prompt` | full | prompt text/files/skills |
| `session.context` | partial | system/messages/options mutable; tools mutable but Effect Schema objects are projected to JSON Schema; plugin-supplied `Schema` objects are rejected |
| `session.model.request` | full | baseURL/headers |
| `session.http.request` / `.response` | full | method/url/headers/body as bytes; streaming bodies not supported (buffered, ≤ 32 MiB) |
| `session.retry` | full | decision only |
| `experimental.idle` | full | may admit synthetic input |
| `permission.evaluate` | full | effect/message mutation |
| `shell.create.before` | full | command/cwd/env/timeout |
| transforms (`agent, command, skill, mcp, provider, model, tool, reference, vcs, websearch, integration`) | full | JSON documents |
| `aisdk.sdk` / `aisdk.language` | **not supported** | requires live objects and npm provider SDKs; provider packages run natively |
| `experimental.ws.*` | not in v1 | no WS providers in our first release |
| `rpc.register` | full | mounted at `/api/rpc/:rpcID/:method` |
| `event.subscribe` | full | read-only events, filtered |
| `storage` / `credential.activate` | full | namespaced KV + credential ops |
| `plugin.list` | full | inventory |
| GUI contributions | web: settings-page/panel only | desktop-only contributions are returned by `GET /api/plugin/gui` for compat but ignored by the browser build |

Anything not listed is rejected at load with `unsupported feature: <name>` and
shown in the inventory; it is never silently ignored.

### Loading rules (upstream-compatible)

- Config: `plugins: (string | {package, options?})[]`.
- Discovery: `<config-root>/plugin/` and `<config-root>/plugins/`, sorted;
  `.ts/.js` files, directories, symlinks.
- Targets: npm `pkg`, `pkg@1.2.3`, git URL, absolute path, `./rel`, `file://`;
  `-pkg` removes; `*`, `prefix.*`, `opencode.*` select known plugins.
- Entrypoints: exports subpaths `server`, `rpc`, `gui-experimental`; else root
  `index.*`; the browser never loads `gui-experimental`.
- Install: delegated to the sidecar (Bun/Node) into
  `<cache>/plugins/<sanitized>/<generation>`, scripts disabled, 7-day
  retention, file lock. The Rust process never runs npm.
- Hot reload: watch the plugin entry graph + config; 100 ms debounce; a failed
  reload keeps the previous generation running.
- Duplicate id: first wins, the rest are `failed` with `duplicate id`.

### Inventory and error semantics

`Plugin.Info` is unchanged:

```jsonc
{"id":"…","source":{"type":"package","target":"…","version":"…"},"features":{"server":true,"rpc":true},"state":{"status":"active"}}
```

- A transform failure disables that plugin and closes its scope; other plugins
  keep running.
- Guarded ids (`opencode.provider.*`, policy plugins) cannot be removed.
- `plugin.updated` is emitted on every inventory change; no payload.
- If the sidecar is down, JS plugins show `failed (sidecar unavailable)` and
  the server remains fully functional.

## 3. HTTP surface

| Route | Behavior |
|---|---|
| `GET /api/plugin` | inventory (native + JS), location-scoped |
| `GET /api/plugin/gui?id=` | GUI bundle for desktop; 404 in web-only builds |
| `POST /api/plugin/check` | check updates (sidecar does registry I/O) |
| `POST /api/plugin/update` | install/update; 503 with per-target failures |
| `POST /api/rpc/:rpcID/:method` | dispatch to native or sidecar RPC |

`rpc.*` events are published with `location` and arbitrary `data`; the SSE
filter accepts `rpc.` prefix.

## 4. Security

- JS plugins are trusted code: they run with the user's privileges in the
  sidecar. This is the same trust model as upstream. Never auto-install from
  untrusted config; `plugins` entries from a project config require confirmation
  (`plugins.allow_project = ask` by default).
- The sidecar does not inherit the server password or provider credentials
  except through explicit `credential` callbacks.
- Native plugins are compiled in; no `dlopen`.
- A plugin cannot intercept another plugin's RPC namespace.

## 5. What we deliberately drop

- Desktop GUI plugin sandbox (`new Function` + Electron main/renderer split).
- TUI plugin entrypoint.
- In-process `aisdk` live-object hooks.
- Per-location plugin graphs: one sidecar, one inventory, location only affects
  transforms that depend on config.
