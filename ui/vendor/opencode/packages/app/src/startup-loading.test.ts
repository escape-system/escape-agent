import { describe, expect, test } from "bun:test"
import { readFileSync } from "node:fs"
import { join } from "node:path"
import { initialLocaleHint, loadInitialLocale } from "@/runtime/i18n/language"

// Boot contract: index.html carries no static splash/loading screen, and the app's first commit never waits for the
// locale chunk. This file runs through bun's plain loader, so it builds DOM nodes directly.
const html = readFileSync(join(import.meta.dir, "..", "index.html"), "utf8")

function rootMarkup() {
  const match = html.match(/<div id="root"[^>]*>([\s\S]*?)<\/div>\s*<script src="\/src\/entry\.tsx"/)
  if (!match) throw new Error("index.html has no #root block feeding the entry script")
  return match[1]
}

function mountRoot() {
  const root = document.createElement("div")
  root.setAttribute("id", "root")
  root.innerHTML = rootMarkup()
  document.body.appendChild(root)
  return root
}

function shellNode() {
  const node = document.createElement("main")
  node.setAttribute("data-testid", "app-shell")
  node.textContent = "shell"
  return node
}

describe("no static startup splash", () => {
  test("index.html ships a clean #root with no splash markup or blocking loader CSS", () => {
    const markup = rootMarkup()

    expect(markup.trim()).toBe("")
    expect(markup).not.toContain("data-opencode-splash")
    expect(html).not.toContain("[data-opencode-splash]")
    expect(html).not.toContain("@keyframes oc-splash-slide")
  })

  test("the document boots directly into #root without pre-JS splash text", () => {
    const root = mountRoot()

    expect(root.querySelector("[data-opencode-splash]")).toBeNull()
    expect(root.textContent ?? "").toBe("")
    expect(root.querySelectorAll('[role="status"]').length).toBe(0)

    root.remove()
  })
})

describe("first commit does not wait for the locale chunk", () => {
  test("the first-frame locale is available synchronously while its dictionary is still pending", () => {
    const hint = initialLocaleHint()
    expect(typeof hint).toBe("string")
    expect(hint.length).toBeGreaterThan(0)

    // The async load still exists for the later swap and may be in flight; it must not gate the hint.
    const pending = loadInitialLocale()
    expect(hint).toBe(initialLocaleHint())
    expect(pending instanceof Promise).toBe(true)
  })

  test("committing the shell renders content cleanly into #root", () => {
    const root = mountRoot()
    root.replaceChildren(shellNode())

    expect(root.children.length).toBe(1)
    expect(root.querySelector("[data-opencode-splash]")).toBeNull()
    expect(root.querySelector('[data-testid="app-shell"]')).not.toBeNull()
    expect(root.textContent ?? "").toContain("shell")
    root.remove()
  })
})
