$ErrorActionPreference = "Stop"

# Builds the web app and installs it at web/dist.
#
# The default source is the vendored copy in ui/vendor/opencode, which already
# contains patches 0001-0004 from ui/patches. If OC_UPSTREAM points at an
# external opencode checkout, that tree must already have those patches applied;
# this script does not apply them.

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent $here
$upstream = if ($env:OC_UPSTREAM) { $env:OC_UPSTREAM } else { Join-Path $root "ui/vendor/opencode" }

if (-not (Get-Command bun -ErrorAction SilentlyContinue)) {
    Write-Error "bun is required to build the web app (docs/13-ui.md)"
    exit 1
}

if (-not (Test-Path (Join-Path $upstream "packages/app/package.json"))) {
    Write-Error "app workspace not found at $upstream/packages/app; set OC_UPSTREAM to an already patched checkout or restore ui/vendor/opencode"
    exit 1
}

if (-not (Test-Path (Join-Path $upstream "bun.lock"))) {
    Write-Error "bun.lock not found at $upstream/bun.lock; the web app workspace must be installed from a committed lockfile"
    exit 1
}

Write-Host "building web app from $upstream"
Push-Location $upstream
try {
    bun install --frozen-lockfile
    if ($LASTEXITCODE -ne 0) { throw "bun install failed (exit $LASTEXITCODE)" }
    bun run --filter "@opencode/app" build
    if ($LASTEXITCODE -ne 0) { throw "vite build failed (exit $LASTEXITCODE)" }
}
finally {
    Pop-Location
}

$appDist = Join-Path $upstream "packages/app/dist"
if (-not (Test-Path $appDist)) {
    Write-Error "build produced no output at $appDist"
    exit 1
}

$dist = Join-Path $root "web/dist"
if (Test-Path $dist) { Remove-Item -Recurse -Force $dist }
New-Item -ItemType Directory -Force -Path $dist | Out-Null
Copy-Item -Path (Join-Path $appDist "*") -Destination $dist -Recurse -Force

bun (Join-Path $root "ui/verify-dist.mjs")
if ($LASTEXITCODE -ne 0) { throw "dist verification failed (exit $LASTEXITCODE)" }

Write-Host "web app written to $dist"
