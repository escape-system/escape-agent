# 11 — LSP

Upstream v2 has only LSP config/UI/events; the runtime was dropped in the
rewrite. We implement it fresh, client-side, with RAM as a hard constraint.

## Config

```jsonc
{
  "lsp": true,
  "lsp_install": false,
  "lsp.servers": {
    "rust": { "command": ["rust-analyzer"], "extensions": [".rs"], "initialization": {} },
    "ts":   { "command": ["typescript-language-server", "--stdio"], "extensions": [".ts", ".tsx"] },
    "deno": { "disabled": true }
  }
}
```

- `lsp: false` disables all; `true` or absent makes built-in servers eligible.
- A map merges over the built-ins by id; `disabled: true` removes one.
- `command` is required for a custom server; `extensions` omitted/empty means
  "match every file" (upstream v2 semantics).
- `env` merges over the process env; `initialization` becomes
  `initializationOptions` and the `workspace/didChangeConfiguration` settings.
- **No auto-download by default.** Servers must be on `PATH`, in project
  `node_modules/.bin`, or configured explicitly. `lsp_install: true` enables
  pinned, checksummed installs (opt-in; supply-chain and disk cost).
- `OPENCODE_DISABLE_LSP_DOWNLOAD` is honored (hard off).

Built-in ids (38, from the v2 fixture): `deno typescript vue eslint oxlint
biome gopls ruby-lsp ty pyright elixir-ls zls csharp razor fsharp sourcekit-lsp
rust clangd svelte astro jdtls kotlin-ls yaml-ls lua-ls php-intelephense prisma
dart ocaml-lsp bash terraform texlab dockerfile gleam clojure-lsp nixd tinymist
haskell-language-server julials`.

## Lifecycle

`LspSupervisor` keyed by `(ServerId, CanonicalRoot)`:

```
Cold → Spawning(shared future) → Ready(Arc<Client>) → Broken{backoff}
```

- Spawn is lazy: the first tool/edit/read of a matching extension.
- Root resolution per server (nearest `Cargo.toml` with `[workspace]`,
  `deno.json`, `tsconfig.json`, `go.mod`, `pyproject.toml`, etc.); extensionless
  files (`Dockerfile`, `Makefile`) match by exact filename, fixing an upstream
  bug.
- Init timeout 45 s; `initialize` → `initialized` → optional
  `workspace/didChangeConfiguration`.
- Capabilities declared: work-done progress, workspace configuration, dynamic
  watched files, `didOpen`/`didChange`, publish-diagnostics version support,
  and (ours) pull diagnostics when the server advertises it.
- Idle TTL 10 min, swept every 60 s; `max_servers` default 4 (config), global
  budget 6. Above the cap, evict the LRU idle client; never evict one with a
  request in flight.
- Failure: `Broken` with exponential backoff (60 s → 5 min → 15 min); no
  permanent blacklist. A missing binary never retries until config changes.
- Shutdown: `shutdown` request (2 s) → `exit` → 1 s wait → kill the process
  tree (Job Object on Windows, process group on Unix).
- One client per root, single-root workspace folder; multi-root is out of
  scope until a server requires it.

## File sync

- First touch: `workspace/didChangeWatchedFiles` (type 1) then
  `textDocument/didOpen` with the mapped `languageId` and version 0.
- Subsequent changes: type 2 + full-text `didChange` with incrementing version.
  Incremental sync is not required; full text is simpler and bounded by file
  size (skip > 2 MiB with a log).
- `didClose` on eviction or when the file is deleted.
- Language map: `.ts→typescript`, `.tsx→typescriptreact`, `.mts→typescript`,
  `.js→javascript`, `.jsx→javascriptreact`, `.rs→rust`, `.py→python`,
  `.go→go`, `.java→java`, `.c→c`, `.cpp→cpp`, `.h→c`, `.hpp→cpp`, `.cs→csharp`,
  `.rb→ruby`, `.php→php`, `.sh→shellscript`, `.yaml/.yml→yaml`, `.json→json`,
  `.md→markdown`, `.toml→toml`, `.lua→lua`, `.nix→nix`, `.tf→terraform`,
  `.tex→latex`, `.dockerfile/Dockerfile→dockerfile`, `.gleam→gleam`,
  `.clj→clojure`, `.hs→haskell`, `.kt→kotlin`, `.dart→dart`, `.swift→swift`,
  `.zig→zig`, `.vue→vue`, `.svelte→svelte`, `.astro→astro`.
- Positions on the wire are UTF-16 code units (LSP default). All Rust-side
  offsets are byte offsets; conversion happens in one place and is tested with
  non-BMP characters.

## Diagnostics

- Push: cache `HashMap<PathBuf, {version, items, at}>`; publish event
  `lsp.client.diagnostics {serverID, path}` on change (debounced 150 ms).
- Pull: after `didOpen`/`didChange`, if advertised, send
  `textDocument/diagnostic` with the previous result id; merge with push
  results; pull failures degrade to push only.
- Wait helper: 150 ms debounce, 3 s timeout, exact path + server.
- `edit`/`write` wait for diagnostics and append only `severity == 1` errors,
  max 20:

```
LSP errors detected in this file:
<diagnostics file="…">
ERROR [line:col] message
</diagnostics>
```

  Full diagnostics go to tool `metadata.diagnostics` (severity, code, source,
  range), not into the model text.
- `read` warms the client (`touchFile(file,false)`) but never waits.
- Cache bounds: 10k paths / 8 MiB per client; cleared on eviction; versioned so
  a stale publish is ignored.

## Tools

`lsp` (permission `lsp`, 1-based `line`/`character`):

```jsonc
{ "operation": "goToDefinition|findReferences|hover|documentSymbol|workspaceSymbol|goToImplementation|prepareCallHierarchy|incomingCalls|outgoingCalls",
  "filePath": "…", "line": 1, "character": 1 }
```

Plus `lsp_diagnostics {path}` returning the cached diagnostics as JSON.
`workspaceSymbol` returns the first 50 (upstream slices 10; we keep 50 with a
truncation marker). References include declarations.

Rename, code actions, completion and formatting are **not** in v1: formatting
stays the external `formatter` command system (upstream behavior); rename/code
actions need a workspace-edit application path that is out of scope until phase
5. The tool returns `unsupported operation` rather than pretending.

## Routes and events

| Route | Response |
|---|---|
| `GET /api/lsp` | `[{id,name,root,status:"connected"\|"starting"\|"error"}]` |
| `GET /api/lsp/diagnostics?path=` | cached diagnostics for a path |

Events: `lsp.updated {}` (status/registry changed) and
`lsp.client.diagnostics {serverID,path}` (restored from v1; additive in v2).

## RAM policy

Child language servers dominate: rust-analyzer 0.2–2 GB, tsserver 150–400 MB,
jdtls 300 MB–1 GB. Therefore:

- No server ships with the binary and none auto-installs by default.
- Lazy spawn, idle eviction, `max_servers`, global budget.
- Heavy servers (`rust`, `jdtls`, `kotlin-ls`, `clangd`, `csharp`) count as 2
  slots; evict them first.
- A server for a root with no open files for 10 min is evicted even under the
  cap if RSS pressure is reported by the OS (best-effort).
- `lsp` status shows RSS per child in the UI when the platform exposes it.

## Security

- Commands come from config or built-ins only; the model cannot spawn a server.
- `initialization` is data; no shell interpolation.
- stdio only; no TCP LSP endpoints in v1.
- File URIs are percent-encoded, canonicalized, and confined to the root;
  symlinks resolved before matching.
