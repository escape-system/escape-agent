# UI build

The web app is [`vendor/opencode/packages/app`](vendor/opencode/packages/app)
(SolidJS + Vite), vendored from upstream opencode. Edit it here; the build
script installs, builds, copies the result to `../web/dist`, and verifies it.

## Requirements

- Bun (upstream requires Bun for install/transforms). Node alone is not enough.
- No sibling checkout is needed. `ui/vendor/opencode` is pre-patched at the
  version recorded in [`UPSTREAM`](UPSTREAM). Set `OC_UPSTREAM` to build a
  different opencode tree; that tree must already have the patches applied.

## Run

```sh
./build.sh          # macOS / Linux / Git Bash
./build.ps1         # Windows PowerShell
```

Output: `../web/dist` (gitignored). `oc-server` serves it from disk in phase 1
and embeds it at release build later.

## Patches

`patches/` holds numbered patches against the vendored `packages/app` and
`packages/session-ui`. They are kept as provenance and for re-vendoring; the
build script no longer applies them because `vendor/opencode` already contains
them. An external `OC_UPSTREAM` must be patched with `git apply --check` first:

1. `0001-settings-tabs.patch` — storage settings tabs (memory, goals, observer,
   subagents, lanes);
2. `0002-session-agent-panels.patch` — session header agent panels: subagent
   tree, observer repair rows and budget banner, composer goal chip, lane pill,
   and the child-session peer drawer.
3. `0003-model-release-unknown.patch` — empty release date when the server
   reports `time.released === 0`.
4. `0004-background-metadata-guard.patch` — tolerate a running shell tool part
   without `metadata`.
5. `0005-attachment-mime-guard.patch` — tolerate attachments and prompt files
   without `mime`/`source`, and read mime from data URLs.
6. `0006-stop-interrupt.patch` — durable Stop: the client store remembers a
   requested stop and reports the session idle until an execution boundary
   settles it, and the composer interrupts once with `resume:false`.
7. `0007-token-counter-live-reset.patch` — the context indicator measures the
   live per-step token count against the compaction ceiling.
8. `0008-settings-quotas.patch` — the per-server `quotas` settings page
   (`settings/quotas/`), the provider quota section in the Usage dialog, and the
   `en`/`tr` strings. The settings tab polls every 60 s; the Usage dialog never
   polls and reads the cached snapshot.
9. `0009-models-control-plane.patch` — the Models settings tab rebuilt as a
   server-backed control plane (fallback chain, per-model quota, notes,
   cost/usage) and search in the setup wizard's model step
   (`settings/models/{models,api}.tsx|ts`, `providers/connect/dialog.tsx`,
   `runtime/i18n/en.ts`).

Patch scope (see `docs/13-ui.md`):

1. settings tabs: memory, goals, observer, subagents, lanes, quotas;
2. session tree + sibling drawer;
3. observer repair rows and goal chip;
4. lane status in the subagent bar;
5. provider quotas, spend and balance in settings and the Usage dialog.

## Endpoints the UI consumes

The agent panels read the experimental routes the Rust server exposes; they
render an inline "unavailable" state instead of placeholder data when a route
answers `503`:

| Panel | Route |
|---|---|
| Subagent tree | `GET /api/experimental/subagent/tree?sessionID=` |
| Peer drawer | `GET/POST`… `POST /api/experimental/subagent/:SID/message`, `subagent.peer.message` |
| Observer rows + banner | `GET /api/experimental/observer`, `GET /api/experimental/observer/repairs` |
| Goal chip | `GET /api/experimental/goal?sessionID=`, `goal.updated` |
| Lane pill | `GET /api/experimental/lane`, `lane.*` |
| Quotas tab + Usage dialog | `GET /api/quotas`, `POST /api/quotas/refresh` |

Quota writes:

| Action | Route |
|---|---|
| Save / clear the OpenRouter management key | `PUT` / `DELETE /api/quotas/openrouter/management-key` |
| Antigravity account select, enable, disable | `POST /api/antigravity/accounts/:id/{select,enable,disable}` |

The antigravity routes are registered by `data::antigravity::routes()` in
`crates/oc-server/src/lib.rs` (merged into the protected router), not inline in
the router body. `GET /api/quotas` answers `503` when the server runs without
the quota service, which the UI renders as an in-page "quota service not
attached" state rather than an error, and it never retries that answer. Only
the settings tab polls (60 s); the Usage dialog passes `enabled: () => false`
and shows whatever the tab already cached.

## Current state

Bun is available on the machine that produced `web/dist` (bun 1.4.4). If the
build machine has no Bun, `build.sh`/`build.ps1` exit before touching `dist`;
do not commit a dist that was not produced by this script.
