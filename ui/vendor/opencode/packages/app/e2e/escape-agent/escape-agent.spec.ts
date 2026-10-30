import { expect, test, type Page, type Request } from "@playwright/test"

const baseURL = process.env.ESCAPE_BASE_URL ?? "http://127.0.0.1:4096"

function watch(page: Page) {
  const errors: string[] = []
  const failed: string[] = []

  page.on("pageerror", (error) => errors.push(`pageerror: ${error.message}`))
  page.on("console", (message) => {
    if (message.type() !== "error") return
    const text = message.text()
    if (/favicon|Failed to load resource|WebSocket|EventSource|net::ERR_ABORTED/i.test(text)) return
    errors.push(`console: ${text}`)
  })
  page.on("response", (response) => {
    if (response.status() < 400) return
    const url = new URL(response.url())
    if (url.origin !== new URL(baseURL).origin) return
    const path = url.pathname
    if (path.startsWith("/api/")) return
    if (/\.(?:js|css|png|woff2?|ico|webmanifest|json)$/.test(path) || path === "/") {
      failed.push(`${response.status()} ${path}`)
    }
  })

  return {
    errors,
    failed,
    async settle() {
      await page.waitForTimeout(500)
      expect(failed, `failed static requests: ${failed.join(", ")}`).toEqual([])
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

async function hasUserMessage(request: Request, sessionID: string) {
  const response = await request.get(`/api/session/${sessionID}/message`)
  if (!response.ok()) return false
  const body = (await response.json()) as { data?: Array<{ type?: string }> }
  return (body.data ?? []).some((message) => message.type === "user")
}

test.describe("Escape Agent end-user readiness", () => {
  test("serves branded static assets and the HTTP/SSE contract", async ({ request }) => {
    const info = await request.get("/api/info")
    expect(info.status()).toBe(200)
    expect((await info.json()).version).toBeTruthy()

    const index = await request.get("/")
    expect(index.status()).toBe(200)
    expect(index.headers()["content-type"]).toContain("text/html")
    expect(await index.text()).toContain("Escape Agent")

    const manifest = await request.get("/site.webmanifest")
    expect(manifest.status()).toBe(200)
    const manifestBody = await manifest.json()
    expect(manifestBody.name).toBe("Escape Agent")
    expect(manifestBody.short_name).toBe("Escape")

    const favicon = await request.get("/icons/dev/favicon.ico")
    expect(favicon.status()).toBe(200)
    expect(favicon.headers()["content-type"]).toContain("image/x-icon")

    const mark = await request.get("/brand/escape-agent-mark.png")
    expect(mark.status()).toBe(200)
    expect(mark.headers()["content-type"]).toBe("image/png")

    const sessions = await request.get("/api/session")
    expect(sessions.status()).toBe(200)
    expect(Array.isArray((await sessions.json()).data)).toBe(true)

    const controller = new AbortController()
    const events = await fetch(new URL("/api/event", baseURL), {
      headers: { accept: "text/event-stream" },
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
    const list = (await (await request.get("/api/session")).json()).data as Array<{ id: string }>
    expect(list.length).toBeGreaterThan(0)

    let sessionID = list[0]!.id
    for (const session of list.slice(0, 12)) {
      if (await hasUserMessage(request, session.id)) {
        sessionID = session.id
        break
      }
    }

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
  const url = process.env.ESCAPE_AUTH_URL
  const password = process.env.ESCAPE_AUTH_PASSWORD
  test.skip(!url || !password, "set ESCAPE_AUTH_URL and ESCAPE_AUTH_PASSWORD")

  await page.goto(url!)
  await expect(page.locator('[data-component="connect-server"]')).toBeVisible()
  await expect(page.locator(".ea-connect-word")).toContainText("ESCAPE")
  await page.getByRole("button", { name: /Use password/i }).click()
  await page.getByLabel("Server address").fill(url!)
  await page.getByLabel("Password").fill(password!)
  await page.getByRole("button", { name: "Connect" }).click()
  await expect(page.locator(".ea-home-hero")).toBeVisible({ timeout: 30_000 })
})
