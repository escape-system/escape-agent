#!/usr/bin/env node
// Enforces the shipped web bundle budgets (docs/15-performance.md, "Web bundle").
// Plain Node, no dependencies. Accepts --dist <path> or OC_WEB_DIST, and defaults
// to <repo>/web/dist.
import { readFileSync, readdirSync, statSync, existsSync } from "node:fs"
import { gzipSync } from "node:zlib"
import path from "node:path"
import { fileURLToPath } from "node:url"

const here = path.dirname(fileURLToPath(import.meta.url))
const repo = path.resolve(here, "../..")

const argv = process.argv.slice(2)
const flag = argv.indexOf("--dist")
const dist =
  flag >= 0 && argv[flag + 1] ? path.resolve(argv[flag + 1]) : path.resolve(process.env.OC_WEB_DIST || path.join(repo, "web/dist"))

const MiB = 1024 * 1024
const KiB = 1024
const BUDGET = {
  mapFiles: 0,
  distBytes: 110 * MiB,
  entryGzipBytes: 220 * KiB,
  modulepreloads: 75,
  swPrecacheBytes: 3 * MiB,
  thirdPartyFaviconHits: 0,
}

const failures = []
const measured = {}
const fail = (msg) => failures.push(msg)

if (!existsSync(dist)) {
  console.error(`check-dist-budget: FAIL\n  dist not found: ${dist}`)
  process.exit(1)
}

function walk(dir, out = []) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name)
    if (entry.isDirectory()) walk(full, out)
    else out.push(full)
  }
  return out
}

const files = walk(dist)
let total = 0
const maps = []
const assets = []
for (const file of files) {
  const size = statSync(file).size
  total += size
  if (file.endsWith(".map")) maps.push(file)
  if (file.includes(`${path.sep}_assets${path.sep}`) && file.endsWith(".js")) assets.push(file)
}
measured.distBytes = total
measured.files = files.length
measured.mapFiles = maps.length

if (maps.length > BUDGET.mapFiles) {
  fail(`${maps.length} *.map file(s) in dist (budget 0); maps are Sentry-only upload artifacts — e.g. ${path.relative(dist, maps[0])}`)
}
if (total > BUDGET.distBytes) {
  fail(`dist is ${(total / MiB).toFixed(1)} MiB (budget 110 MiB)`)
}

const htmlPath = path.join(dist, "index.html")
if (!existsSync(htmlPath)) fail("index.html missing")
const html = existsSync(htmlPath) ? readFileSync(htmlPath, "utf8") : ""

const preloads = (html.match(/rel="modulepreload"/g) || []).length
measured.modulepreloads = preloads
if (preloads > BUDGET.modulepreloads) {
  fail(`${preloads} modulepreload links (budget ${BUDGET.modulepreloads})`)
}

const entryMatch = html.match(/_assets\/index-[^"]*\.js/)
if (!entryMatch) {
  fail("entry chunk _assets/index-*.js not referenced by index.html")
} else {
  const entryPath = path.join(dist, entryMatch[0])
  if (!existsSync(entryPath)) {
    fail(`entry chunk missing on disk: ${entryMatch[0]}`)
  } else {
    const raw = readFileSync(entryPath)
    const gz = gzipSync(raw, { level: 9 }).length
    measured.entry = entryMatch[0]
    measured.entryBytes = raw.length
    measured.entryGzipBytes = gz
    if (gz > BUDGET.entryGzipBytes) {
      fail(`entry chunk gzip is ${(gz / KiB).toFixed(1)} KiB (budget 220 KiB)`)
    }
    if (raw.includes("sourceMappingURL")) fail(`entry chunk contains a sourceMappingURL`)
  }
}

const swPath = path.join(dist, "sw.js")
if (!existsSync(swPath)) {
  fail("sw.js missing")
} else {
  const sw = readFileSync(swPath, "utf8")
  const entries = [...sw.matchAll(/\{url:"([^"]+)",revision:[^}]+\}/g)]
  let precache = 0
  let found = 0
  for (const entry of entries) {
    const file = path.join(dist, entry[1].replace(/^\//, ""))
    if (existsSync(file)) {
      precache += statSync(file).size
      found++
    }
  }
  measured.swBytes = statSync(swPath).size
  measured.swPrecacheEntries = entries.length
  measured.swPrecacheFiles = found
  measured.swPrecacheBytes = precache
  if (entries.length !== found) fail(`sw.js precaches ${entries.length} entries but only ${found} exist in dist`)
  if (precache > BUDGET.swPrecacheBytes) {
    fail(`service-worker precache is ${(precache / MiB).toFixed(2)} MiB (budget 3 MiB)`)
  }
}

// Third-party favicon fetches are the regression this rule exists for. The
// address-bar search template (`https://www.google.com/search?q=`, a URL the user
// navigates to) is not a request the page issues, so only the favicon endpoint fails.
let faviconHits = 0
let otherGoogle = 0
for (const file of assets) {
  const text = readFileSync(file, "utf8")
  faviconHits += (text.match(/google\.com\/s2\/favicons/g) || []).length
  otherGoogle += (text.match(/google\.com(?!\/s2\/favicons)/g) || []).length
}
measured.thirdPartyFaviconHits = faviconHits
measured.otherGoogleMentions = otherGoogle
if (faviconHits > BUDGET.thirdPartyFaviconHits) {
  fail(`${faviconHits} google.com/s2/favicons reference(s) in _assets/*.js (budget 0); icons must come from bundled CSS masks`)
}

if (failures.length) {
  console.error(`check-dist-budget: FAIL (${dist})`)
  for (const message of failures) console.error(`  - ${message}`)
  console.error(`  measured: ${JSON.stringify(measured)}`)
  process.exit(1)
}

console.log(
  `check-dist-budget: OK (${dist}) — ${(measured.distBytes / MiB).toFixed(1)} MiB, 0 maps, entry gzip ${(measured.entryGzipBytes / KiB).toFixed(1)} KiB, ${measured.modulepreloads} preloads, sw precache ${(measured.swPrecacheBytes / MiB).toFixed(2)} MiB`,
)
