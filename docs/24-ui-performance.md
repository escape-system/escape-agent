# 24 — Web UI performance audit

Audit of the served web client, measured on the deployed `web/dist` on
2026-11-07 (Linux x86_64, Release binary). Every number below names the command
that produced it. Where a value was not obtained it says **not measured**; no
number here is estimated.

## Scope

Covers the static payload the Rust server serves, the startup module graph, the
live-serving path and the SSE fan-in that feeds the UI. It does not cover
provider latency, SQLite, or turn-loop cost; those are in
[15-performance](15-performance.md).

Two constraints shaped how this was measured:

- `web/dist` is read from disk per request (`crates/oc-server/src/static_files.rs:19`,
  `tokio::fs::read` at `:53`), selected by `--web-dist` / `OC_WEB_DIST`
  (`crates/oc-cli/src/cli.rs:77-78`, `crates/oc-cli/src/paths.rs:33-35`,
  `crates/oc-server/src/lib.rs:337-341`). UI changes therefore deploy by
  replacing the directory; no server rebuild or restart is involved.
- Restarting `escape-agent.service` kills live agent sessions
  (`KillMode=control-group`). No measurement here restarted the service.
  Latency was taken with read-only `GET`s against the deployed directory and
  reproduced on a scratch server bound to a spare port with a copy of the same
  tree (recipe in [Re-measure](#how-to-re-measure)).

## Architecture as built

| Stage | Location |
|---|---|
| Build driver | `ui/build.sh` → `bun install --frozen-lockfile` → `bun run --filter @opencode/app build` (`packages/app/package.json:20`, vite 8.2.4) |
| Output install | `cp -R packages/app/dist web/dist`, then `bun ui/verify-dist.mjs` |
| Entry | `packages/app/src/entry.tsx` → `src/app.tsx` |
| Router | `src/shell/routes/routes.tsx` (`AppRoutes` at `:50-77`, `<Shell>` at `:112`) |
| SSE client | `src/runtime/server/client.tsx:96` (`flushInterval: 16`) → `packages/client/src/solid/connection.ts:73` (default flush 10 ms) → `packages/client/src/solid/data.ts` |
| Transcript virtualization | `src/session/timeline/virtualizer.tsx` (`@tanstack/solid-virtual`, `fallbackItemSize = 60` at `:34`, `estimateSize` at `:295`, `overscan: 2` at `:188`) |
| Markdown | `packages/session-ui/src/components/markdown.tsx` + `markdown.worker.ts` (worker transport), `markdown-cache.tsx:15` (`const max = 200`) |

The build emits one entry chunk plus one `modulepreload` link for every other
module in its static import closure; the route table itself is lazy
(`lazy()` imports at `routes.tsx:16-33`).

## Measured baseline

Static inventory of `web/dist` (all commands from the repository root):

| Metric | Value | Command |
|---|---|---|
| Total size / files | 150,410,147 B / 5,529 | `du -sb web/dist`; `find web/dist -type f \| wc -l` |
| Sourcemaps | 2,642 files / 61,472,659 B (58.62 MiB) | `find web/dist -name '*.map' -printf '%s\n' \| awk '{s+=$1;n++} END{print n,s}'` |
| JS, all | 2,649 files / 31,408,375 B (29.95 MiB) | `find web/dist -name '*.js' -printf '%s\n' \| awk '{s+=$1;n++} END{print n,s}'` |
| JS in `_assets` | 2,647 files / 31,067,133 B (29.63 MiB) | `find web/dist/_assets -name '*.js' -printf '%s\n' \| awk '{s+=$1;n++} END{print n,s}'` |
| wasm | 4 files / 36,129,122 B (34.45 MiB) | `find web/dist -name '*.wasm' -printf '%s %p\n'` |
| Fonts, TTF | 86 files / 16,264,372 B (15.51 MiB) | `find web/dist -name '*.ttf' -printf '%s\n' \| awk '{s+=$1;n++} END{print n,s}'` |
| Fonts, all formats | 135 files / 19,289,592 B (18.40 MiB) | `find web/dist -type f \( -name '*.woff2' -o -name '*.woff' -o -name '*.ttf' \) -printf '%s\n' \| awk '{s+=$1;n++} END{print n,s}'` |
| `index.html` | 9,610 B, 88 asset refs | see below |

`index.html` refs, and the bytes they pull:

| Group | Count | Bytes | Command |
|---|---|---|---|
| `modulepreload` | 71 | 1,841,820 | `grep -o '<link rel="modulepreload"[^>]*>' web/dist/index.html \| wc -l` |
| `stylesheet` | 16 | 494,636 | `grep -o '<link rel="stylesheet"[^>]*>' web/dist/index.html \| wc -l` |
| module script (entry) | 1 | 763,305 | `grep -o '<script[^>]*type="module"[^>]*>' web/dist/index.html` |
| **Total** | **88** | **3,099,761** | per-file `stat`, summed |

| Metric | Value | Command |
|---|---|---|
| Static import closure of the entry | 72 modules / 2,605,125 B | import-graph walk from `_assets/index-C0PiWap_.js` over static `import`/`export … from` |
| Closure, gzip-9 | 786,646 B | `gzip -9 -c` over each module, summed |
| Serial first load | 89 requests / 3,109,371 B raw / 869,785 B gzip-9 | 88 refs + `index.html`; `gzip -9 -c` per file, summed |
| `sw.js` precache | 2,816 entries / 37,591,783 B (35.85 MiB) | parse `url:"…"` from `web/dist/sw.js`, `stat` each |
| Lazy chunks (not in `index.html`) | 2,575 files / 28,462,008 B | `_assets/*.js` minus the 71 preloads and the entry |
| Duplicate hashed stems (JS) | 316 groups, 7,352,618 B (7.01 MiB) | strip `-<8 chars>.js`, sum all but the largest per group |

The 71 preloads are exactly the closure minus the entry chunk (verified set
equality), so the preload list is generated from the startup graph, not hand
maintained.

Live serving, 3 runs each, median of `time_total`:

| Path | Status | Content-Length | Median | Command |
|---|---|---|---|---|
| `/` | 200 `text/html` | 9,610 | 2.96 ms | `curl -sS -o /dev/null -w '%{http_code} %{content_type} %{size_download} %{time_total}' URL` |
| `/_assets/index-C0PiWap_.js` | 200 `text/javascript` | 763,305 | 5.82 ms | same |
| `/_assets/index-Bjbfb--7.css` | 200 `text/css` | 436,612 | 5.16 ms | same |
| `/api/info` | 200 `application/json` | 108 | 2.96 ms | same, with `-u opencode:$PASS` |
| `/api/event` SSE TTFB | 200 `text/event-stream` | — | 5.0–5.6 ms | `curl -N -w '%{time_starttransfer}'` |

A fresh 3-run pass on a spare-port copy of the same tree reproduced the same
order: `/` 4.7–10.6 ms, entry 3.3–13.7 ms, CSS 7.0–19.7 ms, `/api/info`
3.5–4.5 ms, SSE TTFB 5.1–10.6 ms. Absolute milliseconds at this scale are
dominated by scheduling; the payload sizes are the durable signal.

First SSE frame is `server.connected` on every run
(`data: {"id":"evt_…","type":"server.connected","data":{}}`).

Response headers, checked with `curl -sS -D -` on GET (not HEAD — this server
returns `content-length: 0` for HEAD):

- No `etag`.
- No `last-modified`.
- No `content-encoding`, including when the request sends
  `Accept-Encoding: gzip, br`; the entry chunk is still 763,305 B on the wire.
- Cache rules match [13-ui](13-ui.md): `no-cache` for `/` and `sw.js`,
  `public, max-age=31536000, immutable` for `_assets/**`.
- Sourcemaps are served publicly: `GET /_assets/index-C0PiWap_.js.map` →
  `200 text/plain`, 1,975,881 B, no credentials.

## Ranked findings

Ordered by payload removed per unit of change. Each is reported, not fixed
here.

| # | Finding | Location | Mechanism |
|---|---|---|---|
| a | Production sourcemaps shipped | `packages/app/vite.config.ts:52` (`sourcemap: true`) | 2,642 `.map` files, 61,472,659 B, 40.9 % of `web/dist`. Served publicly; the Sentry plugin only deletes maps when `SENTRY_AUTH_TOKEN` is set, and it is unset |
| b | Zero static compression | `Cargo.toml:85` (`tower-http` features `cors`, `trace` only); layers at `crates/oc-server/src/lib.rs:566-578`; rule at `docs/13-ui.md:19` | No `CompressionLayer`. First load is 3,109,371 B raw; the same bytes at gzip-9 are 869,785 B, a 72.0 % reduction the client never sees |
| c | Four 3 s polling queries | `src/session/agent-panels/agent-queries.ts:63,90,113,136` | `refetchInterval: 3_000` on subagent tree, repairs, observer state and goal: 80 requests/min per open session panel, contradicting `docs/13-ui.md:177-178` (`refetchOnMount:false`, `staleTime: 30s`) |
| d | `effect@4.0.6` in the startup graph | chunk `_assets/dist-DOFOGWOn.js`, 806,807 B (247,541 B gzip-9), 217 sources all under `effect@4.0.6`; imported from `src/session/timeline/message-timeline.tsx:3`, `virtualizer.tsx:10`, `controller.tsx:7`, `src/shell/state/layout.tsx:2`, `src/shell/commands/command.tsx:5`, `src/shell/tabs/schema.ts:3` | The chunk is in the 72-module static closure and is its largest module, ahead of the 763,305 B entry. `Predicate.isTagged` is used in `message-timeline.tsx` and `virtualizer.tsx`; `controller.tsx` uses `Predicate.isString`; the shell files use `Schema`/`SchemaGetter` |
| e | Third-party favicon fetch per external link | `packages/session-ui/src/components/markdown.tsx:373` | Renders `https://www.google.com/s2/favicons?domain=…` per rendered external link: a cross-origin request, and a third-party dependency in a local-first UI |
| f | One Solid root per code block | `packages/session-ui/src/components/markdown.tsx:150-170` (`render()` at `:156`), called from `:1084` and `:1172` | Every code block mounts its own reactive root for a copy button instead of sharing one component tree |
| g | PWA precaches the entire build | `packages/app/vite.pwa.ts:34,36` (`globPatterns: ["**/*"]`, `maximumFileSizeToCacheInBytes: Number.MAX_SAFE_INTEGER`) | `sw.js` install fetches 2,816 entries / 37,591,783 B, including every lazy chunk and every 34 MiB of wasm |
| h | 71 eager `modulepreload` refs | eager imports at `src/shell/routes/routes.tsx:3,8,9,10` and `src/app.tsx:13,20`; the lazy route table is `routes.tsx:50-77` | Shell, Home, `CommandProvider`, `SettingsProvider` and the layout/settings providers are imported at module scope, so the whole startup closure is preloaded before the first route renders. The route table itself is lazy and is not the cause |

## What is not a bottleneck

Recorded so this is not re-investigated:

- Transcript virtualization already exists and is applied per row
  (`virtualizer.tsx`, TanStack `createVirtualizer`, `overscan: 2`). Long
  transcripts do not render all rows.
- Markdown parsing is already off the main thread (`markdown.worker.ts`), with
  a 200-entry HTML cache (`markdown-cache.tsx:15`), block-level hashing
  (`dataset.markdownHash` compare/skip at `markdown.tsx:1026-1040`) and
  incremental spans from the persistent block parser
  (`packages/patchmark/src/blocks.ts`, streaming tail forks only the open
  spine). The remaining markdown cost is the per-block Solid root in finding
  (f), not parsing.
- SSE fan-out is a server-side concern and is not the UI limit: 208 events/s
  observed on one subscriber (303–336 events per 1.5 s window), ~69 KB/s
  uncompressed while a session streams. The client batches at 16 ms
  (`runtime/server/client.tsx:96`), which is below the event rate that would
  need coalescing.
- The entry chunk (763,305 B) and the 72-module closure (2,605,125 B) are
  normal for a SolidJS + TanStack Query app, but one third-party package is
  31 % of that closure (806,807 B). The problem is that the closure is
  uncompressed and eagerly preloaded, not that it exists.

## What we explicitly do not change

Per [13-ui](13-ui.md) "What we do not change": the transcript renderer, tool
renderer, composer, permissions/form modals, file tree, terminal and routes
stay upstream. Findings (e) and (f) are inside the markdown renderer and are
therefore reported only; a fix there needs an upstream-shaped change, not a
fork patch. Findings (a), (b), (g) and (h) are build/serve-level and do not
touch component behavior.

## Open items reported, not fixed

| Item | Evidence |
|---|---|
| `bench/baseline.json` referenced by the perf doc is absent | `docs/15-performance.md:99`; `ls bench/` → no `baseline.json` |
| Docs say four UI patches, `ui/patches/` has five | `docs/13-ui.md:28,43`; `ls ui/patches/*.patch \| wc -l` → 5, including `0005-attachment-mime-guard.patch` |
| `expect_seconds` documented for the subagent tree does not exist in `src` | `docs/13-ui.md:87-88`; `grep -rn 'expect_seconds\|expectSeconds' packages/*/src` → no matches |
| `memory.updated` documented as a reducer case appears nowhere in `src` | `docs/13-ui.md:176`; `grep -rn 'memory\.updated' packages/*/src` → no matches |
| `agent-queries.ts` uses `refetchOnMount:true` / 3 s instead of the documented `false` / 30 s | `src/session/agent-panels/agent-queries.ts:63,90,113,136` vs `docs/13-ui.md:177-178` |

## How to re-measure

Run from the repository root.

Dist inventory:

```sh
du -sb web/dist
find web/dist -type f | wc -l
find web/dist -name '*.map' -printf '%s\n' | awk '{s+=$1;n++} END{print n,s}'
find web/dist -name '*.js' -printf '%s\n' | awk '{s+=$1;n++} END{print n,s}'
find web/dist -name '*.wasm' -printf '%s %p\n'
```

Compression comparison (what the wire would be with gzip):

```sh
gzip -9 -c web/dist/_assets/index-C0PiWap_.js | wc -c   # 201344
# whole first load: index.html + the 88 refs, gzip -9 -c each, summed
```

Spare-port server (never restart the live unit):

```sh
cp -R web/dist /tmp/ui-baseline-dist
target/release/opencode serve --hostname 127.0.0.1 --port <spare> \
  --web-dist /tmp/ui-baseline-dist --password <pw> \
  --db /tmp/ui-baseline-data/scratch.db --directory /tmp
curl -sS -o /dev/null -w '%{http_code} %{size_download} %{time_total}\n' \
  http://127.0.0.1:<spare>/
```

Playwright gotcha: the cached browser is `chromium-1243`, and the harness must
pass it explicitly plus container-safe flags:

```js
chromium.launch({
  executablePath: "/root/.cache/ms-playwright/chromium-1243/chrome-linux64/chrome",
  args: ["--no-sandbox", "--disable-dev-shm-usage"],
})
```

## Budgets

Browser bundle budgets are being introduced by a companion change in
[15-performance](15-performance.md); see that file for the enforced thresholds.
This document records the pre-change baseline those budgets are compared
against. No threshold is stated here, because none is enforced yet.

### Measured after change

Built 2026-11-07 from `ui/vendor/opencode/packages/app` (`bun run build`, wall 72.12 s, exit 0) and deployed to `web/dist`.

| Metric | Before | After |
|---|---|---|
| `*.map` files in dist | 2,642 | 0 |
| dist bytes | 150,410,147 | 88,493,398 |
| dist files | 5,529 | 2,887 |
| entry chunk | `_assets/index-C0PiWap_.js` 763,305 B | `_assets/index-D0SVV0lg.js` 763,262 B |
| entry gzip -9 | 201,832 B | 201,297 B |
| `sourceMappingURL` in entry | present | 0 |
| `rel="modulepreload"` links | 71 | 71 |
| index.html asset refs | 88 | 88 |
| index.html assets, raw | 3,109,371 B | 3,095,864 B |
| index.html assets, gzip -9 total | not measured | 864,267 B |
| `sw.js` | 340,093 B | 17,604 B |
| SW precache entries / bytes | 2,816 / 37,591,783 | 19 / 1,447,817 |
| `google.com/s2/favicons` in `_assets/*.js` | 1 | 0 |

Budgets from `ui/perf/check-dist-budget.mjs` now gate the build; see `docs/15-performance.md`.
