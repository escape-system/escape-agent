#!/usr/bin/env bun
// Verifies that web/dist is a complete, servable build produced by ui/build.sh.
// Exits non-zero with a list of problems instead of letting the server serve a broken app.
import { readFileSync, readdirSync, statSync } from "node:fs"
import { dirname, join, resolve, sep } from "node:path"
import { fileURLToPath } from "node:url"

const root = dirname(dirname(fileURLToPath(import.meta.url)))
const dist = resolve(root, "web", "dist")
const problems = []

function checkFile(path, label) {
  try {
    const stat = statSync(path)
    if (!stat.isFile()) problems.push(`${label} is not a file: ${path}`)
    else if (stat.size === 0) problems.push(`${label} is empty: ${path}`)
    return stat
  } catch {
    problems.push(`${label} is missing: ${path}`)
    return undefined
  }
}

const indexPath = join(dist, "index.html")
const index = checkFile(indexPath, "index.html")
const html = index ? readFileSync(indexPath, "utf8") : ""

if (index && html.length === 0) problems.push("index.html is empty")

const placeholder = html.match(/%OPENCODE_[A-Z0-9_]*%/g)
if (placeholder) {
  problems.push(`index.html still contains unexpanded placeholders: ${[...new Set(placeholder)].join(", ")}`)
}

const refs = [...html.matchAll(/\b(?:src|href)\s*=\s*["']([^"']+)["']/g)].map((match) => match[1])
let assetCount = 0
for (const ref of refs) {
  if (!ref.startsWith("/") || ref.startsWith("//")) continue
  const path = ref.split(/[?#]/, 1)[0]
  if (!path || path === "/") continue
  let relative
  try {
    relative = decodeURIComponent(path.slice(1))
  } catch {
    problems.push(`index.html references an undecodable path: ${ref}`)
    continue
  }
  const file = resolve(dist, relative)
  if (file !== dist && !file.startsWith(dist + sep)) {
    problems.push(`index.html references a path outside web/dist: ${ref}`)
    continue
  }
  assetCount += 1
  checkFile(file, `asset referenced by index.html (${ref})`)
}

for (const required of ["sw.js", "site.webmanifest", "icons/dev/favicon.ico"]) {
  checkFile(join(dist, required), required)
}

try {
  const assets = readdirSync(join(dist, "_assets"))
  if (!assets.some((name) => /^index-.+\.js$/.test(name))) {
    problems.push("web/dist/_assets has no index-*.js entry bundle")
  }
} catch {
  problems.push(`web/dist/_assets is missing: ${join(dist, "_assets")}`)
}

if (problems.length > 0) {
  console.error(`verify-dist: FAILED (${problems.length} problem${problems.length === 1 ? "" : "s"})`)
  for (const problem of problems) console.error(`  - ${problem}`)
  process.exit(1)
}

console.log(`verify-dist: OK (${assetCount} root-relative assets, ${join(dist, "_assets")} present)`)
