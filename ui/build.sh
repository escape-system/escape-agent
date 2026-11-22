#!/usr/bin/env sh
set -eu

# Builds the web app and installs it at web/dist.
#
# The default source is the vendored copy in ui/vendor/opencode, which already
# contains patches 0001-0004 from ui/patches. If OC_UPSTREAM points at an
# external opencode checkout, that tree must already have those patches applied;
# this script does not apply them.

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
root=$(CDPATH= cd -- "$here/.." && pwd)
upstream=${OC_UPSTREAM:-"$root/ui/vendor/opencode"}

if ! command -v bun >/dev/null 2>&1; then
  echo "error: bun is required to build the web app (docs/13-ui.md)" >&2
  exit 1
fi

if [ ! -f "$upstream/packages/app/package.json" ]; then
  echo "error: app workspace not found at $upstream/packages/app" >&2
  echo "set OC_UPSTREAM=/path/to/opencode (already patched) or restore ui/vendor/opencode" >&2
  exit 1
fi

if [ ! -f "$upstream/bun.lock" ]; then
  echo "error: bun.lock not found at $upstream/bun.lock" >&2
  echo "the web app workspace must be installed from a committed lockfile" >&2
  exit 1
fi

echo "building web app from $upstream"
cd "$upstream"

bun install --frozen-lockfile
bun run --filter @opencode/app build

if [ ! -d "$upstream/packages/app/dist" ]; then
  echo "error: build produced no output at $upstream/packages/app/dist" >&2
  exit 1
fi

rm -rf "$root/web/dist"
mkdir -p "$root/web/dist"
cp -a "$upstream/packages/app/dist/." "$root/web/dist/"

# ghostty-web's terminal renderer loads `ghostty-vt.wasm` at runtime from
# `/ghostty-vt.wasm` (see the bundled Ghostty.load()). The app build does not
# copy it into dist, so without this the terminal pane mounts but the VT engine
# fails to load and no shell ever starts. Ship it at the document root.
ghostty_wasm=$(find "$upstream/node_modules" -path '*/ghostty-web/dist/ghostty-vt.wasm' -type f -print -quit 2>/dev/null || true)
if [ -z "$ghostty_wasm" ]; then
  ghostty_wasm=$(find "$upstream/node_modules" -name 'ghostty-vt.wasm' -type f -print -quit 2>/dev/null || true)
fi
if [ -n "$ghostty_wasm" ]; then
  cp "$ghostty_wasm" "$root/web/dist/ghostty-vt.wasm"
  echo "shipped terminal WASM: ghostty-vt.wasm ($(wc -c < "$root/web/dist/ghostty-vt.wasm") bytes)"
else
  echo "error: ghostty-vt.wasm not found under $upstream/node_modules; terminal will not work" >&2
  exit 1
fi

bun "$root/ui/verify-dist.mjs"
echo "checking bundle budgets"
node "$root/ui/perf/check-dist-budget.mjs" --dist "$root/web/dist"
echo "web app written to $root/web/dist"
