# Vendored opencode web app

Source snapshot of the upstream opencode frontend used by `ui/build.sh` and
`ui/build.ps1`.

- Upstream: <https://github.com/anomalyco/opencode>
- Version: 2.2.0 (see [`UPSTREAM`](../UPSTREAM))
- License: MIT (see [`LICENSE`](LICENSE))

This tree is the editable copy of the app. Change HTML/TSX here and rebuild; no
sibling `../opencode` checkout is needed.

## What is included

- Root manifests: `package.json` (trimmed: no scripts, no root dependencies),
  `bun.lock` (regenerated for this reduced workspace), `bunfig.toml`,
  `tsconfig.json`, `LICENSE`, `.gitignore`, and `patches/` (upstream dependency
  patches referenced by `patchedDependencies`).
- Workspaces: `app`, `client`, `schema`, `protocol`, `gui-plugins`, `plugin`,
  `plugin-browser`, `session-ui`, `ui`, `util`, `patchmark`, `ai`,
  `http-recorder`, `theme`, `httpapi-codegen`.
- `packages/desktop/icons/` only. `packages/app/vite.icons.ts` reads the
  favicon and PWA icons from there at build time; the rest of the desktop app is
  not part of the web build.

`node_modules/`, `dist/`, `.git/`, and test reports are not vendored.

Applied UI patches (`../patches`, kept for provenance):

- `0001-settings-tabs.patch`
- `0002-session-agent-panels.patch`
- `0003-model-release-unknown.patch`
- `0004-background-metadata-guard.patch`

The snapshot already contains all four. The build scripts deliberately do not
apply patches; an external `OC_UPSTREAM` checkout must be patched beforehand.

## Updating

1. Pick the upstream commit for the target version and update [`UPSTREAM`](../UPSTREAM).
2. Copy the same root files and workspace packages listed above, excluding
   `node_modules`, `dist`, `.git`, and test reports.
3. Apply `../patches/*.patch` in name order to the copied tree
   (`git apply --check` first). If upstream drifted, regenerate the patch and
   keep `../patches` as the provenance record.
4. Re-trim the root `package.json` (keep only `name`, `version`, `private`,
   `type`, `packageManager`, `license`, `workspaces`, `overrides`,
   `patchedDependencies`, `trustedDependencies`; drop `scripts`,
   `dependencies`, and `devDependencies`).
5. Regenerate the lock for the reduced workspace:

   ```sh
   bun install          # in ui/vendor/opencode
   ```

6. Build and verify:

   ```sh
   bun run --filter @opencode/app build
   cd ../.. && ./ui/build.sh
   ```

Commit `bun.lock` with the source so `bun install --frozen-lockfile` works.
