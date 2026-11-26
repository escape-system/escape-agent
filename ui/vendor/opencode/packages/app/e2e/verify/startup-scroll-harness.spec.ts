import { expect, test } from "@playwright/test"
import type { OpenCodeEvent } from "@opencode/client/promise"
import { SERVER, sessionHref } from "../utils/app"
import { mockWorkspace } from "../utils/workspace"
import { inboxEnqueued, textPart, userMessage } from "../utils/timeline"
import { prepareSession, timelineScroller } from "./agentic-stubs"

// Verification-only specs (DRAFTED, NOT EXECUTED: source writers were still
// landing when these were written). Run them only after root activation:
//
//   cd ui/vendor/opencode/packages/app
//   PLAYWRIGHT_BUILD=1 bun run test:e2e -- e2e/verify --workers=1 --retries=0 --reporter=line
//
// Scope (clauses with no other evidence):
//   1. the static splash paints before the JS bundle and is removed by the app;
//   2. a failing API never leaves an indefinite blank screen (visible retry);
//   3. a queued synthetic/peer row survives a reload (durable inbox re-read);
//   4. programmatic scroll correction does not start history loading, while
//      explicit user intent does (the reader-position complaint);
//   5. the mobile queue strip is visible at 320/390 px with no overflow.
//
// Locator risks recorded in the handoff: the retry control's accessible name
// and the peer-drawer trigger are not yet pinned to a shipped data attribute,
// so tests 2 and 3 assert the documented contract (a visible control) and are
// expected RED until W-STARTUP / the drawer owner lands.

const sessionID = "ses_verify_startup"
const otherID = "ses_verify_startup_other"

/**
 * The tall history fixtures append filler lines to every prompt, so `exact` text no longer
 * matches. The leading `^`+word boundary keeps "Prompt 3" from matching "Prompt 35".
 */
const prompt = (n: number) => new RegExp(`^Prompt ${n}\\b`)

/**
 * Wheel up the loaded history with real gestures until one more older page is requested, then stop.
 * A fixed step budget cannot be used: after a prepend the virtualizer keeps the reader's row in
 * place, so the offset grows by the whole older page and a fixed walk ends up inside the prepended
 * page, where the boundary row this test names is no longer mounted. The deadline reports the real
 * offset instead of failing on an unexplained locator timeout.
 */
async function wheelUntilOlderPage(
  page: import("@playwright/test").Page,
  scroller: ReturnType<typeof timelineScroller>,
  requests: URL[],
  before: number,
  /** Extra stop condition, for a page whose rows must be mounted before the walk ends. */
  mounted?: () => Promise<boolean>,
) {
  await scroller.hover()
  const deadline = Date.now() + 20_000

  while (Date.now() < deadline) {
    if (requests.length > before && (!mounted || (await mounted()))) return
    await page.mouse.wheel(0, -400)
    await page.waitForTimeout(80)
  }

  const at = await scroller.evaluate((element) => element.scrollTop)
  throw new Error(`no older page request within 20s (scrollTop=${at})`)
}

/**
 * The timeline row whose box crosses the scrollport's top edge, i.e. what the reader is looking
 * at. Prepended history must keep this row in place instead of scrolling the view to the oldest
 * message.
 */
function topRow(scroller: ReturnType<typeof timelineScroller>) {
  return scroller.evaluate((element) => {
    const rootTop = element.getBoundingClientRect().top
    // Mobile timelines render a 16px `h-4` spacer above message 0; when an upward wheel reaches
    // scrollTop=0 before prepend, the anchored row stays at +16px while the prepended row above it
    // ends at +16px. Threshold past that spacer so the anchored row itself is measured.
    const row = [...element.querySelectorAll<HTMLElement>("[data-timeline-key]")].find(
      (item) => item.getBoundingClientRect().bottom > rootTop + 20,
    )

    return row?.dataset.timelineKey ?? null
  })
}

test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true, serviceWorkers: "block" })

test("the app boots directly into the composer without a static splash", async ({ page }) => {
  const workspace = await mockWorkspace(page, {
    name: "VerifyStartup",
    sessions: [{ id: sessionID, title: "Verify startup" }],
    pageMessages: () => ({
      items: [userMessage([textPart("prt_splash_0", "hello")], { id: "msg_splash_0", created: 1_000 })],
    }),
  })

  const started = Date.now()
  await page.goto(sessionHref(sessionID, workspace.server), { waitUntil: "commit" })
  const splash = page.locator("[data-opencode-splash]")
  await expect(splash).toHaveCount(0)

  // The app renders the real composer directly without any full-screen loading gate.
  const editor = page.locator('[data-component="composer-editor"]')
  await expect(editor).toBeVisible({ timeout: 30_000 })
  await expect(splash).toHaveCount(0)
  test.info().annotations.push({ type: "startup-ms", description: String(Date.now() - started) })
  await page.screenshot({ path: test.info().outputPath("startup-ready.png") })
})

test("a failing API shows a visible error with retry instead of a blank screen", async ({ page }) => {
  // Registered here, after the mock's catch-all, so every API call fails.
  await mockWorkspace(page, {
    name: "VerifyStartupFail",
    sessions: [{ id: sessionID, title: "Verify startup fail" }],
    pageMessages: () => ({ items: [] }),
  })
  await page.route("**/api/**", (route) => route.abort("failed"))

  const started = Date.now()
  await page.goto(sessionHref(sessionID, SERVER))

  // Contract: within a bounded wait the screen offers a control to retry, or an
  // explicit error region. A dark, empty body fails this.
  const retry = page.getByRole("button", { name: /retry|try again|reconnect|reload/i }).first()
  const errorRegion = page.locator('[role="alert"], [data-component*="error"]').first()
  await expect(retry.or(errorRegion)).toBeVisible({ timeout: 20_000 })
  const bodyText = await page.locator("body").innerText()
  expect(bodyText.trim().length).toBeGreaterThan(0)
  test.info().annotations.push({ type: "error-visible-ms", description: String(Date.now() - started) })
  await page.screenshot({ path: test.info().outputPath("startup-error.png") })
})

test("a queued synthetic/peer row stays visible in the queue across a reload", async ({ page }) => {
  const inboxID = "msg_verify_startup_queued"
  const queuedRow = {
    id: inboxID,
    sessionID,
    time: { created: 1_020 },
    type: "synthetic" as const,
    payload: { text: "peer note while busy", peer: true },
    delivery: "queue" as const,
  }

  await prepareSession(page, {
    sessionID,
    title: "Verify startup queue",
    sessions: [{ id: sessionID, title: "Verify startup queue" }],
    pageMessages: () => ({
      items: [userMessage([textPart("prt_startup_q0", "first")], { id: "msg_startup_q0", created: 1_000 })],
    }),
    sessionStatus: () => ({ [sessionID]: { type: "running" } }),
    inbox: () => [queuedRow],
  })

  const strip = page.locator('[data-component="session-queue-panel"]')
  await expect(strip).toBeVisible()
  // queue.ts widened `queued()` to synthetic rows: this row must not be dropped.
  await expect(strip.getByText("peer note while busy", { exact: true })).toBeVisible()
  await page.screenshot({ path: test.info().outputPath("queue-synthetic.png") })

  // Durable visibility: the row is server state, so a refresh re-renders it.
  await page.reload()
  await expect(strip).toBeVisible()
  await expect(strip.getByText("peer note while busy", { exact: true })).toBeVisible()
  await page.screenshot({ path: test.info().outputPath("queue-synthetic-after-reload.png") })
})

test("programmatic scroll does not load older history; user intent does", async ({ page }) => {
  // Tall rows on purpose: five one-line prompts do not overflow an 844px viewport, so every
  // scrollTop write clamps to 0 and the old test could never observe a scroll at all.
  const filler = Array.from({ length: 10 }, (_, index) => `filler line ${index}`).join("\n\n")
  const pageOf = (index: number) =>
    Array.from({ length: 12 }, (_, offset) => {
      const n = index * 12 + offset
      return userMessage([textPart(`prt_old_${n}`, `Prompt ${n}\n\n${filler}`)], {
        id: `msg_old_${n}`,
        created: 1_000 + n,
      })
    })

  const older: URL[] = []
  page.on("request", (request) => {
    const url = new URL(request.url())
    // The client pages history with the server's opaque `cursor`; `before` is the decoded form the
    // mock server hands to `pageMessages`. Matching only `before` would count no request at all and
    // make the "zero older requests" half trivially true.
    if (
      /^\/api\/session\/[^/]+\/message$/.test(url.pathname) &&
      (url.searchParams.has("before") || url.searchParams.has("cursor"))
    )
      older.push(url)
  })

  await prepareSession(page, {
    sessionID,
    title: "Verify scroll intent",
    sessions: [
      { id: sessionID, title: "Verify scroll intent" },
      { id: otherID, title: "Verify scroll intent other" },
    ],
    pageMessages: (_session, _limit, before) => {
      if (before === "oldest") return { items: pageOf(0) }
      if (before === "older") return { items: pageOf(1), cursor: "oldest" }
      return { items: pageOf(2), cursor: "older" }
    },
  })

  const scroller = timelineScroller(page)
  await expect(scroller.locator("[data-timeline-virtual-content]")).toBeVisible()
  await expect(scroller.getByText(prompt(35))).toBeVisible()

  // Prove the located node is the real scrollport before asserting on offsets: a locator
  // aimed at the ScrollView wrapper (overflow: hidden) can never move and would make both
  // halves of this test vacuous.
  const geometry = await scroller.evaluate((element) => ({
    overflowY: getComputedStyle(element).overflowY,
    scrollTop: element.scrollTop,
    scrollHeight: element.scrollHeight,
    clientHeight: element.clientHeight,
  }))
  expect(["auto", "scroll"]).toContain(geometry.overflowY)
  expect(geometry.scrollHeight).toBeGreaterThan(geometry.clientHeight + 1_000)

  const beforeProgrammatic = older.length

  // A pinned view deliberately re-pins a bare offset write: following the newest message must not
  // be broken by a stray programmatic scroll. So first stop following the way a reader does, with a
  // real wheel from the end. A synthetic WheelEvent does not scroll, so it leaves the intent armed
  // for whatever write comes next; the real gesture produces the scroll that spends it.
  await scroller.hover()
  await page.mouse.wheel(0, -400)
  await expect.poll(() => scroller.evaluate((element) => element.scrollTop)).toBeGreaterThan(1_000)
  expect(older.length).toBe(beforeProgrammatic)

  // Restore/anchor style correction: while stopped, a programmatic write into the top zone with no
  // user intent must not page history. The invariant asserted here is the request count, not where
  // the offset settles: the virtualizer legitimately re-anchors a write into an unmeasured region,
  // but that re-anchoring must never be mistaken for intent.
  await scroller.evaluate((element) => {
    element.scrollTop = 40
  })
  await page.waitForTimeout(1_000)
  expect(older.length).toBe(beforeProgrammatic)
  const readingRow = await topRow(scroller)
  expect(readingRow).not.toBeNull()

  // Explicit intent must load the older page: wheel upward like a reader until the top is reached.
  // The final wheel arrives where nothing can scroll, so no scroll event follows and the gesture
  // branch in virtualizer.tsx is the only path that can carry the intent into loadOlder. Asserted
  // on the request log: prepending legitimately moves the offset by the older page's height, so
  // the offset is not a valid "reached the top" signal once the page has landed.
  await wheelUntilOlderPage(page, scroller, older, beforeProgrammatic, async () => (await scroller.getByText(prompt(23)).count()) > 0)
  await expect.poll(() => older.length, { timeout: 10_000 }).toBeGreaterThan(beforeProgrammatic)
  // The older page really is the middle page, i.e. the first page's cursor was followed.
  await expect(scroller.getByText(prompt(23)).first()).toBeVisible()

  // The reading position held: the same row still crosses the viewport's top edge and the offset
  // is positive, i.e. the older rows were prepended above the reader instead of throwing the view
  // to the oldest message (the original "timeline jumps to the beginning" complaint).
  await expect.poll(() => topRow(scroller)).toBe(readingRow)
  expect(await scroller.evaluate((element) => element.scrollTop)).toBeGreaterThan(0)
  // And the prepended page is in the transcript, not merely requested.
  await expect(scroller.getByText(prompt(24)).first()).toBeVisible()
})

test("the queue strip fits 320 and 390 px without horizontal overflow", async ({ page }) => {
  for (const width of [320, 390] as const) {
    await page.setViewportSize({ width, height: 844 })
    const prepared = await prepareSession(page, {
      sessionID,
      title: "Verify startup width",
      sessions: [{ id: sessionID, title: "Verify startup width" }],
      pageMessages: () => ({
        items: [userMessage([textPart("prt_w_0", "hi")], { id: "msg_w_0", created: 1_000 })],
      }),
      sessionStatus: () => ({ [sessionID]: { type: "running" } }),
      width,
      height: 844,
    })
    const event: OpenCodeEvent = inboxEnqueued({
      sessionID,
      inboxID: `msg_verify_w_${width}`,
      item: { type: "user", payload: { text: `queued at ${width}` }, delivery: "queue" },
    })
    await prepared.pushEvents([event])

    const strip = page.locator('[data-component="session-queue-panel"]')
    await expect(strip).toBeVisible()
    await expect(strip.getByText(`queued at ${width}`, { exact: true })).toBeVisible()

    const overflow = await page.evaluate(() => ({
      document: document.documentElement.scrollWidth - window.innerWidth,
      body: document.body.scrollWidth - window.innerWidth,
    }))
    expect(overflow.document).toBeLessThanOrEqual(1)
    expect(overflow.body).toBeLessThanOrEqual(1)
    await page.screenshot({ path: test.info().outputPath(`queue-${width}.png`) })
  }
})
