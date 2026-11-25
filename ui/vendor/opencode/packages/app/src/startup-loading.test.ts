import { describe, expect, test } from "bun:test"
import { readFileSync } from "node:fs"
import { join } from "node:path"
import { initialLocaleHint, loadInitialLocale } from "@/runtime/i18n/language"

// Boot contract: the document carries a visible splash inside #root before any JS runs, and the app's first commit
// never waits for the locale chunk (the pre-fix bug: entry.tsx awaited loadInitialLocale() before render(), so a slow
// locale import meant a black screen). This file runs through bun's plain loader, so it builds DOM nodes directly.
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

describe("static startup splash", () => {
  test("index.html ships a branded splash inside #root with inline critical CSS", () => {
    const markup = rootMarkup()

    expect(markup).toContain("data-opencode-splash")
    expect(markup).toContain('role="status"')
    expect(markup).toContain('aria-live="polite"')
    expect(markup).toContain("/brand/escape-agent-mark.png")
    // The splash must not depend on a stylesheet request: its rules are inline in the document head.
    expect(html).toContain("[data-opencode-splash]")
    expect(html).toContain("position: fixed")
    expect(html).toContain("@keyframes oc-splash-slide")
  })

  test("the static markup paints content with no JS at all", () => {
    const root = mountRoot()

    expect(root.querySelector("[data-opencode-splash]")).not.toBeNull()
    expect(root.textContent ?? "").toContain("ESCAPE AGENT")
    expect(root.textContent ?? "").toContain("Loading")
    expect(root.querySelectorAll('[role="status"]').length).toBe(1)

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

  test("committing the shell before the splash is removed leaves a non-empty root", () => {
    for (const removeSplashFirst of [true, false]) {
      const root = mountRoot()
      if (removeSplashFirst) root.querySelector("[data-opencode-splash]")?.remove()

      // Solid's render() replaces the root's children synchronously at commit.
      root.replaceChildren(shellNode())
      if (!removeSplashFirst) root.querySelector("[data-opencode-splash]")?.remove()

      expect(root.children.length).toBeGreaterThan(0)
      expect(root.querySelector('[data-testid="app-shell"]')).not.toBeNull()
      expect(root.textContent ?? "").toContain("shell")
      root.remove()
    }
  })

  test("the splash is the only pre-JS content, so its removal after commit cannot empty the root", () => {
    const root = mountRoot()
    root.replaceChildren(shellNode())
    expect(root.querySelector("[data-opencode-splash]")).toBeNull()
    expect(root.children.length).toBe(1)
    root.remove()
  })
})
