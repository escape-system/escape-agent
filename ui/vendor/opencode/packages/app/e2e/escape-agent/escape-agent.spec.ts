import { expect, test, type Page, type Request } from "@playwright/test"

const baseURL = process.env.ESCAPE_BASE_URL ?? "http://127.0.0.1:8788"
// The live server's origin is also the sign-in URL, so a password alone (no
// ESCAPE_AUTH_URL) is enough to exercise the connect flow.
const authURL = process.env.ESCAPE_AUTH_URL ?? baseURL

// Raw fetch() calls (and any request the fixture does not attach credentials to)
// authenticate with the same Basic pair the browser would send.
const authHeaders: Record<string, string> = process.env.ESCAPE_AUTH_PASSWORD
  ? {
      Authorization:
        "Basic " +
        Buffer.from(
          `${process.env.ESCAPE_AUTH_USER ?? "opencode"}:${process.env.ESCAPE_AUTH_PASSWORD}`,
        ).toString("base64"),
    }
  : {}

function watch(page: Page) {
  const errors: string[] = []
  const failed: string[] = []
  // In-flight requests, minus the long-lived SSE stream, so settle() can wait
  // for the app's own fetches instead of sleeping.
  const pending = new Set<Request>()
  page.on("request", (request) => {
    const path = new URL(request.url()).pathname
    if (path === "/api/event" || path === "/event") return
    pending.add(request)
  })
  const finished = (request: Request) => pending.delete(request)
  page.on("requestfinished", finished)
  page.on("requestfailed", finished)
  // A request issued on a route that the test then navigates away from is
  // aborted by the navigation, and Playwright does not reliably emit
  // requestfailed for those. Without this clear they would sit in `pending`
  // forever and deadlock settle(). Requests started before a navigation cannot
  // say anything about the page that replaced it, so drop them.
  page.on("framenavigated", (frame) => {
    if (frame === page.mainFrame()) pending.clear()
  })

  page.on("pageerror", (error) => errors.push(`pageerror: ${error.message}`))
  page.on("console", (message) => {
    if (message.type() !== "error") return
    const text = message.text()
    if (/favicon|Failed to load resource|WebSocket|EventSource|net::ERR_ABORTED/i.test(text)) return
    errors.push(`console: ${text}`)
  })
  page.on("response", (response) => {
    const status = response.status()
    if (status < 400) return
    const url = new URL(response.url())
    if (url.origin !== new URL(baseURL).origin) return
    const path = url.pathname
    // Only two /api/ 4xx are expected: the pre-auth Basic challenge (the
    // browser replays the request with credentials) and the experimental
    // fs/goal probes this build may not expose, in both their REST and plugin
    // RPC spellings. Everything else is a failure.
    if (status === 401) return
    if (status === 404 && /^\/api\/(?:experimental\/(?:fs|goal)|rpc\/opencode\.(?:fs|goal))(?:\/|$)/.test(path))
      return
    if (path.startsWith("/api/")) {
      failed.push(`${status} ${path}`)
      return
    }
    if (/\.(?:js|css|png|woff2?|ico|webmanifest|json)$/.test(path) || path === "/") {
      failed.push(`${status} ${path}`)
    }
  })

  return {
    errors,
    failed,
    async settle() {
      // Deterministic replacement for a fixed sleep: fonts are loaded, the
      // document is complete, the app root is painted, and no request the
      // app made is still in flight (the SSE stream is excluded above).
      await page.evaluate(() => document.fonts.ready.then(() => undefined))
      await page.waitForFunction(() => document.readyState === "complete")
      await expect(page.locator("#root")).toBeVisible()
      await expect.poll(() => pending.size, { timeout: 10_000 }).toBe(0)
      expect(failed, `failed requests: ${failed.join(", ")}`).toEqual([])
      expect(errors, `page errors: ${errors.join("\n")}`).toEqual([])
    },
  }
}

async function openNewSession(page: Page) {
  const mobile = (page.viewportSize()?.width ?? 1440) <= 767
  if (mobile) {
    await page.getByRole("button", { name: /Tabs/i }).click()
    await page.locator('[data-action="mobile-tabs-new-session"]').click()
  } else {
    await page.getByRole("button", { name: "New session", exact: true }).first().click()
  }
  await expect(page).toHaveURL(/\/new-session/)
  await expect(page.locator('[data-component="new-session"]')).toBeVisible()
}

// The desktop app gates the shell behind its own server registry: a fresh
// browser profile starts on the connect screen. Signing in through the same
// password flow the auth test exercises unlocks the home shell.
async function signIn(page: Page) {
  const url = authURL
  const password = process.env.ESCAPE_AUTH_PASSWORD
  if (!password) return
  await page.goto(url)
  const connect = page.locator('[data-component="connect-server"]')
  const hero = page.locator(".ea-home-hero")
  await expect(connect.or(hero).first()).toBeVisible()
  // Either gate means the app is up; only a visible hero proves the registry
  // already holds a working session, so that is the single early exit.
  if (await hero.isVisible()) return
  await page.getByRole("button", { name: /Use password/i }).click()
  await page.getByLabel("Server address").fill(url)
  await page.getByLabel("Password").fill(password)
  await page.getByRole("button", { name: "Connect" }).click()
  await expect(page.locator(".ea-home-hero")).toBeVisible({ timeout: 30_000 })
}

test.describe("Escape Agent end-user readiness", () => {
  test("serves branded static assets and the HTTP/SSE contract", async ({ request }) => {
    const info = await request.get("/api/info", { headers: authHeaders })
    expect(info.status()).toBe(200)
    expect((await info.json()).version).toBeTruthy()

    const index = await request.get("/", { headers: authHeaders })
    expect(index.status()).toBe(200)
    expect(index.headers()["content-type"]).toContain("text/html")
    expect(await index.text()).toContain("Escape Agent")

    const manifest = await request.get("/site.webmanifest", { headers: authHeaders })
    expect(manifest.status()).toBe(200)
    const manifestBody = await manifest.json()
    expect(manifestBody.name).toBe("Escape Agent")
    expect(manifestBody.short_name).toBe("Escape")

    const favicon = await request.get("/icons/dev/favicon.ico", { headers: authHeaders })
    expect(favicon.status()).toBe(200)
    expect(favicon.headers()["content-type"]).toContain("image/x-icon")

    const mark = await request.get("/brand/escape-agent-mark.png", { headers: authHeaders })
    expect(mark.status()).toBe(200)
    expect(mark.headers()["content-type"]).toBe("image/png")

    const sessions = await request.get("/api/session", { headers: authHeaders })
    expect(sessions.status()).toBe(200)
    expect(Array.isArray((await sessions.json()).data)).toBe(true)

    const controller = new AbortController()
    const events = await fetch(new URL("/api/event", baseURL), {
      headers: { accept: "text/event-stream", ...authHeaders },
      signal: controller.signal,
    })
    expect(events.status).toBe(200)
    expect(events.headers.get("content-type")).toContain("text/event-stream")
    const reader = events.body!.getReader()
    const first = await reader.read()
    controller.abort()
    expect(new TextDecoder().decode(first.value)).toContain("server.connected")
  })

  test("home shows a centered Escape Agent hero on a glass panel", async ({ page }) => {
    const watchdog = watch(page)
    await signIn(page)
    await page.goto("/")
    await expect(page.locator(".ea-home-hero")).toBeVisible()
    await expect(page.locator(".ea-home-hero-title")).toHaveText("ESCAPE AGENT")
    await expect(page.locator(".ea-titlebar-brand")).toContainText("ESCAPE")

    const layout = await page.evaluate(() => {
      const hero = document.querySelector(".ea-home-hero")!.getBoundingClientRect()
      const panel = getComputedStyle(document.querySelector('[data-slot="home-panel"]')!)
      return {
        centerDelta: Math.abs(hero.left + hero.width / 2 - innerWidth / 2),
        backdrop: panel.backdropFilter,
        background: panel.backgroundColor,
        overflow: document.documentElement.scrollWidth - innerWidth,
      }
    })
    expect(layout.centerDelta).toBeLessThanOrEqual(1.5)
    expect(layout.backdrop).toContain("blur")
    expect(layout.background).not.toBe("rgb(14, 14, 14)")
    expect(layout.overflow).toBeLessThanOrEqual(1)

    await page.getByRole("button", { name: "Settings" }).first().click()
    await page.getByRole("tab", { name: "About" }).click()
    await expect(page.locator(".ea-about")).toBeVisible()
    await expect(page.locator(".ea-about")).toContainText("independent")
    await expect(page.locator(".ea-about")).not.toContainText("Missouri")
    await watchdog.settle()
  })

  test("new session shows the Escape wordmark, glass composer, and accepts input", async ({ page }) => {
    const watchdog = watch(page)
    await signIn(page)
    await page.goto("/")
    await expect(page.locator(".ea-home-hero")).toBeVisible()
    await openNewSession(page)

    await expect(page.locator(".ea-wordmark-brandline")).toHaveText("AGENT")
    expect(await page.locator('[data-slot="wordmark-flat"] path').count()).toBeGreaterThanOrEqual(12)

    const composer = page.locator('form[data-component="composer"]').first()
    await expect(composer).toBeVisible()
    await expect(composer).toHaveCSS("backdrop-filter", /blur/)
    const editor = composer.locator('[contenteditable="true"]').first()
    await editor.click()
    await page.keyboard.type("Escape Agent smoke test")
    await expect(editor).toContainText("Escape Agent smoke test")
    await watchdog.settle()
  })

  test("an existing session renders without a double user-message border", async ({ page, request }) => {
    const watchdog = watch(page)
    // Create an empty session instead of depending on seeded history: no model
    // turn runs, so this stays cheap and billable-free.
    const created = await request.post("/api/session", {
      headers: authHeaders,
      data: { title: "e2e-existing-session" },
    })
    expect(created.status()).toBe(200)
    const sessionID = ((await created.json()) as { data: { id: string } }).data.id
    expect(sessionID).toBeTruthy()

    await signIn(page)
    const encoded = Buffer.from(baseURL).toString("base64")
    await page.goto(`/server/${encoded}/session/${sessionID}`)
    await expect(page.locator('[data-slot="session-chat-panel"]')).toBeVisible()
    await expect(page.locator(".ea-titlebar-brand")).toContainText("ESCAPE")

    const tab = page.locator("[data-titlebar-tab]").first()
    const frame = await page.locator('[data-slot="titlebar-tab-item"]').first().evaluate((element) => {
      return getComputedStyle(element).boxShadow
    })
    expect(frame).toContain("inset")
    await tab.evaluate((element) => element.setAttribute("data-status", "busy"))
    const spinner = tab.locator('[data-slot="tab-status"] [data-component="activity-ring"]').first()
    await expect(spinner).toBeVisible()
    const spinnerColor = await spinner.evaluate((element) => getComputedStyle(element).color)
    expect(spinnerColor).not.toBe("rgb(141, 135, 123)")

    // The created session has no user message, so the double-border check can
    // only run when one is present; otherwise it is honestly not exercised.
    const bubble = page.locator('[data-slot="user-message-text"]').first()
    if (await bubble.count()) {
      await expect(bubble).toBeVisible()
      const outer = await page.locator('[data-component="user-message"]').first().evaluate((element) => {
        const style = getComputedStyle(element)
        return { background: style.backgroundColor, border: style.borderTopWidth, shadow: style.boxShadow }
      })
      expect(outer.background).toBe("rgba(0, 0, 0, 0)")
      expect(outer.border).toBe("0px")
      expect(outer.shadow).toBe("none")
    }
    await watchdog.settle()
  })

  test("mobile home keeps the hero, hides the desktop brand, and does not overflow", async ({ page }) => {
    const watchdog = watch(page)
    await page.setViewportSize({ width: 390, height: 844 })
    await signIn(page)
    await page.goto("/")
    await expect(page.locator(".ea-home-hero")).toBeVisible()
    await expect(page.locator(".ea-titlebar-brand")).toBeHidden()

    const overflow = await page.evaluate(() => document.documentElement.scrollWidth - innerWidth)
    expect(overflow).toBeLessThanOrEqual(1)

    await openNewSession(page)
    await expect(page.locator('form[data-component="composer"]')).toBeVisible()
    const sessionOverflow = await page.evaluate(() => document.documentElement.scrollWidth - innerWidth)
    expect(sessionOverflow).toBeLessThanOrEqual(1)
    await watchdog.settle()
  })
})

test("password connect screen brands the server and signs in", async ({ page }) => {
  const url = authURL
  const password = process.env.ESCAPE_AUTH_PASSWORD
  test.skip(!password, "set ESCAPE_AUTH_PASSWORD")

  await page.goto(url!)
  await expect(page.locator('[data-component="connect-server"]')).toBeVisible()
  await expect(page.locator(".ea-connect-word")).toContainText("ESCAPE")
  await page.getByRole("button", { name: /Use password/i }).click()
  await page.getByLabel("Server address").fill(url!)
  await page.getByLabel("Password").fill(password!)
  await page.getByRole("button", { name: "Connect" }).click()
  await expect(page.locator(".ea-home-hero")).toBeVisible({ timeout: 30_000 })
})
