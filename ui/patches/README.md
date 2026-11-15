# Patches

Numbered patches applied to the upstream tree at the version in `../UPSTREAM`.
They are kept for provenance and re-vendoring; `ui/vendor/opencode` already
contains them, and `build.sh`/`build.ps1` do not apply them.

- `0001-settings-tabs.patch` — storage settings tabs (memory, goals, observer,
  subagents, lanes) and their `Config.Patch` writes.
- `0002-session-agent-panels.patch` — session header agent panels: the subagent
  tree, observer repair rows with the budget banner, the composer goal chip,
  the lane pill, and the child-session peer drawer.
- `0003-model-release-unknown.patch` — the model adapter leaves `release_date`
  empty when the server reports `time.released === 0`, so config-defined models
  without a release date stay visible in the model picker.
- `0004-background-metadata-guard.patch` — the session background tracker reads
  a running tool part's `metadata`/`input` optionally, so a `shell` call that is
  still running without metadata no longer crashes the session page.
- `0005-attachment-mime-guard.patch` — guards message attachments and composer prompt
  extraction against missing `mime` and `source`, and extracts mime from data URLs so
  image attachments without explicit mime fields don't throw TypeError on startsWith.
- `0006-stop-interrupt.patch` — durable stop intent in the data store and composer,
  so a stopped session reads idle even when a lagging event or racing status write
  would otherwise make it look busy again.
- `0007-token-counter-live-reset.patch` — the client stores the per-step `tokens`
  from `session.usage.updated` as `session.lastStep` and clears it on
  `session.compaction.ended`; the context indicator and tab prefer that live value,
  fall back to `lastAssistantWithUsage`, and measure usage against the context
  ceiling (`window − max(15% of window, 16000)`) instead of the raw window, so 100%
  of the denominator is the hard limit, not the point where compaction fires.
  Compaction triggers below the ceiling by the buffer: `ceiling − buffer`, where an
  unset (or `0`) `compaction.buffer` means `max(8_000, window/10)`.
- `0008-settings-quotas.patch` — the `quotas` settings page: the per-server tab
  (`settings/route.ts`, `pages.ts`, `shell.tsx`, `search-catalog.ts`,
  `route.test.ts`), the new `settings/quotas/` directory (`api.ts` with the typed
  `/api/quotas` client, formatters and the status vocabulary, `quotas.tsx`,
  `quotas.css`, `api.test.ts`), the provider quota section in the Usage dialog,
  and the `en`/`tr` strings. The dialog reads the cached query only
  (`enabled: () => false`); the settings tab is the only poller (60 s).
- `0009-models-control-plane.patch` — the Models settings tab rebuilt as the
  agent model-policy editor, plus a registry-driven chat model picker.
  `settings/models/api.ts` is the typed client for `GET /api/models/registry`
  and `PUT /api/config/models`; `chain.ts` holds the fallback-chain algebra and
  `models.tsx` renders it: a scope selector (global vs. one agent, grouped
  primary agents / subagents), per-agent chain editing with add/remove/reorder
  and an "already added" filter, whole-chain model visibility, per-model quota,
  cost and usage columns, notes, and collapse/expand-all, plus an unavailable
  registry badge instead of a restart gate. The picker is registry-driven:
  `providers/models/policy.ts` (pure visibility/ordering rules),
  `select-dialog.tsx` (registry-gated visibility, chain-aware ordering,
  favorites first) and the setup wizard's model step
  (`providers/connect/dialog.tsx`). `settings/models/chain.test.ts`,
  `api.test.ts` and `providers/models/picker-policy.test.ts` cover the rules;
  `runtime/i18n/en.ts` and `tr.ts` carry the new `settings.models.*` strings.
  Also carries the provider account/key controls this wave needed
  (`settings/providers/accounts.ts`, `account-actions.ts`, `providers.tsx` —
  "Add backup key" / "Make primary") and the quota client changes in
  `settings/quotas/api.ts`, `quotas.tsx`. HEAD predates 0009, so HEAD is a true
  "before" baseline and the `diff -ruN a b` recipe below applies unchanged:
  20 files, 5432 patch lines. Verified with `git apply --check -p1` on a
  pristine `ui/vendor/opencode` archive and confirmed to reconstruct the
  working tree byte-for-byte (`diff -ru` empty).

Rules:

- one concern per patch, `git format-patch` style, generated from the pinned
  upstream;
- every patch must apply with `git apply --check` on a clean tree;
- no patch may change the API contract; server work lands first.

Regenerating 0002:

1. Back up the patched tree (`a/`), edit the upstream sources, copy the result
   to `b/`.
2. `diff -ruN a b > 0002-session-agent-panels.patch` with both roots holding
   `packages/app/src/...`, so hunk paths carry the `a/` and `b/` prefixes.
3. `git apply --check -p1` on a tree that already has 0001.

Regenerating 0008 (`diff -ruN a b` against a pristine tree, new files included):

```sh
# a/ = HEAD, b/ = the edited working tree; both rooted at packages/app/src.
tmp=$(mktemp -d)
mkdir -p "$tmp/a/packages/app" "$tmp/b/packages/app"
git archive HEAD ui/vendor/opencode/packages/app/src | tar -x -C "$tmp/a/packages/app" --strip-components=5
cp -a ui/vendor/opencode/packages/app/src/. "$tmp/b/packages/app/src/"
cd "$tmp" && diff -ruN a b > 0008-settings-quotas.patch
# prove it on a pristine tree, never by reverting the working copy:
clean=$(mktemp -d) && git archive HEAD ui/vendor/opencode | tar -x -C "$clean"
cd "$clean/ui/vendor/opencode" && git apply --check -p1 /path/to/0008-settings-quotas.patch
```

