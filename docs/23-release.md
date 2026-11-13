# 23 — Release

A release is a `vX.Y.Z` git tag plus a GitHub Release carrying multi-platform archives:
`opencode-<version>-linux-x86_64.tar.gz`, `opencode-<version>-windows-x86_64.tar.gz`,
`opencode-<version>-windows-x86_64.zip`, their `.sha256` sidecars, and `SHA256SUMS`.
Each archive contains the `oc-cli` server binary (`opencode` or `opencode.exe`),
the built web UI at `web/dist`, `README.md`, and `LICENSE`.

Layout inside the archive:

```
opencode-0.3.8-linux-x86_64/   (or opencode-0.3.8-windows-x86_64/)
  opencode                     (or opencode.exe)
  README.md
  LICENSE
  web/dist/…
  web/dist/index.html
```

`web/dist` sits next to the binary on purpose: the server discovers the UI
exe-relatively (`crates/oc-cli/src/paths.rs:34-58`, `web/dist/index.html`), so an
extracted archive serves the app without `--web-dist`. The flag and
`OC_WEB_DIST` (`crates/oc-cli/src/cli.rs:77-78`) still override it.

`web/dist` is git-ignored (`.gitignore:2:/web/dist`); it is built in CI, never
committed.

## Triggers

Two paths, one workflow (`.github/workflows/release.yml`):

1. **Push a tag** — a `v*` tag starts `verify` → `build and publish`.
2. **Manual** — `workflow_dispatch` with a `tag` input, to re-publish a tag that
   already exists (a failed publish, a corrected runner image, deleted assets).
   The input tag must already exist in the repo.

Both paths run the same code: `verify` executes `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets -- -D warnings` and
`cargo test --workspace`; `build` then builds the UI (`bash ui/build.sh`, bun
`1.4.4`, `bun install --frozen-lockfile`), builds
`cargo build --release -p oc-cli --bin opencode`, checks the binary reports the
tag version, packages, and publishes.

`permissions: contents: write` is granted to the workflow; no third-party
release action is used, only `gh release create`. Re-running on an existing tag
replaces the release (`gh release delete --yes` then create).

## The version-must-match rule

The tag must equal `[workspace.package] version` in `Cargo.toml` (currently
`0.3.8`) exactly, minus the leading `v`. The workflow fails before building if
they differ:

```
::error::Cargo.toml version (0.1.0) != tag (0.1.1); bump [workspace.package] version first
```

A tag like `nonsense` or `0.1.0` (no `v`) is rejected earlier by the
`^v[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$` check. Pre-release suffixes are
allowed (`v0.2.0-rc.1`), as long as the Cargo version string matches verbatim.

To release, bump the version and refresh the lockfile so CI sees it:

```sh
# edit [workspace.package] version = "0.3.8" in Cargo.toml
cargo metadata --format-version 1 > /dev/null   # refreshes Cargo.lock
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
git add Cargo.toml Cargo.lock
git commit -m "chore(release): 0.3.8"
git push origin main
```

## Tagging

```sh
git tag -a v0.3.8 -m "v0.3.8"
git push origin v0.3.8
```

The tag push triggers the workflow. Watch it with `gh run list` / `gh run watch`.
No manual asset upload; do not attach files by hand.

To re-publish without a new tag:

```sh
gh workflow run release.yml -f tag=v0.3.8
```

## Running a released build

```sh
tar xzf opencode-0.3.8-linux-x86_64.tar.gz
cd opencode-0.3.8-linux-x86_64
sha256sum -c ../opencode-0.3.8-linux-x86_64.tar.gz.sha256   # if the sidecar is alongside
./opencode serve --hostname 0.0.0.0 --port 8788
```

The server finds `web/dist` next to the binary. Auth on the HTTP surface is
HTTP Basic with user `opencode` and `OPENCODE_SERVER_PASSWORD` from the
environment (`/etc/escape-agent.env` for the systemd deployment).

## Not gated

These three are pre-existing red in this checkout and are deliberately excluded
from both workflows; adding them would make every run fail for reasons
unrelated to the change under test:

| Command | Status | Reason |
|---|---|---|
| `bun run typecheck:tsc` | 445 errors | The vendored `packages/stories` workspace is absent from the checkout, so its imports cannot resolve. |
| `bun run typecheck:e2e` | 459 errors | Same missing `packages/stories` surface plus e2e typings that depend on it. |
| `bun run test:unit` | 929 pass / 1 fail | One stale assertion in `src/settings/search-results.test.ts`. |

CI gates exactly the commands that are green: `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`,
the `oc-bench idle-rss` performance gate (Linux only), and the UI build
(`bash ui/build.sh` → `ui/verify-dist.mjs`).

## Operational notes

- The Windows `rust` leg is visible but non-blocking (`continue-on-error`):
  several modules use unix-only APIs (`oc-cli/src/service.rs`,
  `oc-mcp/src/transport/stdio.rs`, `oc-pty/src/shell.rs`,
  `oc-runtime/src/lane/runner.rs`, `oc-server/src/data/shells/*`). Flip
  `experimental: false` in `.github/workflows/ci.yml` once those are cfg-gated.
- Toolchain is `dtolnay/rust-toolchain@stable` with no `rust-toolchain` file; the
  workspace declares `rust-version = "1.90"` and `resolver = "3"`.
- Both workflows set `permissions:` and `concurrency:`; CI cancels superseded
  runs, releases do not (`cancel-in-progress: false`).
