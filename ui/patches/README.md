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
