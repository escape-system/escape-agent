import { expect, test } from "@playwright/test"
import { assistantMessage, inboxDelivered, textPart, userMessage } from "../utils/timeline"
import { prepareSession, timelineScroller } from "./agentic-stubs"

// Verification-only specs (DRAFTED during the reserved phase, NOT executed).
//
// Scope: the two queue/history contracts this wave changed.
//   1. A prompt that lives only in the durable inbox shows in the queue strip,
//      and delivery promotes it into the transcript with no duplicate and no
//      reordering.
//      Harness limit stated honestly: `pending.sync` materializes every inbox
//      row locally and the timeline hides rows still queued, so the visible
//      delivery transition is what E2E can observe. The narrower
//      "delivered while its local row was evicted" race in data.ts cannot be
//      constructed deterministically through this harness; it needs the client
//      regression test that is still missing
//      (client/test/solid-inbox-delivery.test.ts).
//   2. `message.sync` after `loadMore` keeps the older page and the cursor, so
//      the reader is not thrown back to the first page.
//
// Events go through the harness `inboxEnqueued`/`inboxDelivered` builders: they
// attach the `durable` envelope the generated event union requires. Their
// aggregateID is the timeline fixture's session, which the client reducer does
// not read (it keys on `data.sessionID`), so a foreign session id is fine here.

const sessionID = "ses_verify_queue_delivery"
const otherID = "ses_verify_queue_other"

/**
 * The tall history fixtures append filler lines to every prompt, so `exact` text no longer
 * matches. The leading `^`+word boundary keeps "Prompt 3" from matching "Prompt 35".
 */
const prompt = (n: number) => new RegExp(`^Prompt ${n}\\b`)

/**
 * Wheel up the loaded history with real gestures until exactly one more older page is requested,
 * then stop. A fixed step budget cannot be used: after a prepend the virtualizer keeps the reader's
 * row in place, so the offset grows by the whole older page and the walk either overshoots the page
 * its assertion names or stops short of the top zone. The deadline reports the real offset.
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

test.use({ viewport: { width: 1200, height: 900 }, serviceWorkers: "block" })

test("a queued prompt shows in the strip and delivery promotes exactly one transcript row in place", async ({
  page,
}) => {
  const inboxID = "msg_verify_queued"
  const queuedRow = {
    id: inboxID,
    sessionID,
    time: { created: 1_020 },
    type: "user" as const,
    payload: { text: "queued behind work" },
    delivery: "queue" as const,
  }

  const prepared = await prepareSession(page, {
    sessionID,
    title: "Verify queue delivery",
    sessions: [{ id: sessionID, title: "Verify queue delivery" }],
    pageMessages: () => ({
      items: [userMessage([textPart("prt_vqd_0", "first prompt")], { id: "msg_vqd_user_0", created: 1_000 })],
    }),
    // Busy, so a queued prompt waits behind the running turn instead of steering.
    sessionStatus: () => ({ [sessionID]: { type: "running" } }),
    inbox: () => [queuedRow],
  })

  const scroller = timelineScroller(page)
  await expect(scroller.locator("[data-timeline-virtual-content]")).toBeVisible()

  // Queued rows belong to the strip, not the transcript.
  const strip = page.locator('[data-component="session-queue-panel"]')
  await expect(strip).toBeVisible()
  await expect(strip.getByText("queued behind work", { exact: true })).toBeVisible()
  await expect(
    scroller.locator('[data-timeline-row="UserMessage"]', { hasText: "queued behind work" }),
  ).toHaveCount(0)

  // Durable delivery promotes the row into the transcript.
  await prepared.pushEvents([inboxDelivered({ sessionID, inboxID })])

  const delivered = scroller.locator('[data-timeline-row="UserMessage"]', { hasText: "queued behind work" })
  await expect(delivered).toHaveCount(1)
  await expect(delivered).toBeVisible()

  // Promotion is idempotent: a replayed delivery must not duplicate the row.
  await prepared.pushEvents([inboxDelivered({ sessionID, inboxID })])
  await expect(delivered).toHaveCount(1)

  // The delivered row keeps chronological position instead of jumping to the tail.
  const order = await scroller
    .locator('[data-timeline-row="UserMessage"]')
    .evaluateAll((rows) => rows.map((element) => element.getAttribute("data-message-id")))
  expect(order).toEqual(["msg_vqd_user_0", inboxID])
})

test("message.sync after loadMore keeps the older page and the remaining cursor", async ({ page }) => {
  // Tall rows on purpose: the previous five one-line prompts never overflowed a 900px viewport,
  // so every scrollTop write clamped to 0 and neither the gesture nor the sync could be observed.
  const filler = Array.from({ length: 10 }, (_, index) => `filler line ${index}`).join("\n\n")
  const pageOf = (index: number) =>
    Array.from({ length: 12 }, (_, offset) => {
      const n = index * 12 + offset
      return userMessage([textPart(`prt_sync_${n}`, `Prompt ${n}\n\n${filler}`)], {
        id: `msg_sync_${n}`,
        created: 1_000 + n,
      })
    })

  const seenBefore: Array<string | undefined> = []

  await prepareSession(page, {
    sessionID,
    title: "Verify sync restore",
    sessions: [
      { id: sessionID, title: "Verify sync restore" },
      { id: otherID, title: "Verify sync other" },
    ],
    pageMessages: (_session, _limit, before) => {
      seenBefore.push(before)
      if (before === "oldest") return { items: pageOf(0) }
      if (before === "older") return { items: pageOf(1), cursor: "oldest" }
      return { items: pageOf(2), cursor: "older" }
    },
  })

  const scroller = timelineScroller(page)
  await expect(scroller.locator("[data-timeline-virtual-content]")).toBeVisible()
  await expect(scroller.getByText(prompt(35))).toBeVisible()

  // The located node must be the real scrollport, otherwise every offset write below clamps to
  // 0 and this test would pass or fail for the wrong reason.
  const geometry = await scroller.evaluate((element) => ({
    overflowY: getComputedStyle(element).overflowY,
    scrollHeight: element.scrollHeight,
    clientHeight: element.clientHeight,
  }))
  expect(["auto", "scroll"]).toContain(geometry.overflowY)
  expect(geometry.scrollHeight).toBeGreaterThan(geometry.clientHeight + 1_000)

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

  // A bare programmatic write is re-pinned by design while the view follows the newest message, so
  // first stop following the way a reader does: a real wheel from the end. A synthetic WheelEvent
  // does not scroll and would leave the intent armed for the next programmatic write.
  await scroller.hover()
  await page.mouse.wheel(0, -400)
  await expect.poll(() => scroller.evaluate((element) => element.scrollTop)).toBeGreaterThan(1_000)
  expect(older.length).toBe(0)

  // Load the middle page the way a reader does: a programmatic write into the top zone must never
  // page (asserted on the request count, since the virtualizer legitimately re-anchors the write),
  // then a real Home key reaches the top and its scroll event carries the intent.
  await scroller.evaluate((element) => {
    element.scrollTop = 40
  })
  await page.waitForTimeout(500)
  expect(older.length).toBe(0)
  await wheelUntilOlderPage(page, scroller, older, 0)
  await expect(scroller.getByText(prompt(23))).toBeVisible({ timeout: 10_000 })
  // Count dynamically: loadOlder may legitimately chain a second page from the same gesture, so a
  // later request count alone is not proof that the post-remount walk paged at all.
  const firstPage = older.length

  // Sit mid-history so the remount below has a cached offset to restore. A synthetic WheelEvent
  // scrolls nothing and would only leave intent armed for whatever programmatic write comes next.
  await scroller.hover()
  await page.mouse.wheel(0, 700)
  await expect.poll(() => scroller.evaluate((element) => element.scrollTop)).toBeGreaterThan(300)

  // Leaving and returning re-selects the session, which runs `message.sync`.
  await page.locator(`[data-slot="titlebar-tabs"] a[href*="${otherID}"]`).click()
  await expect(page.getByRole("heading", { name: "Verify sync other" })).toBeVisible()
  await page.locator(`[data-slot="titlebar-tabs"] a[href*="${sessionID}"]`).click()
  await expect(scroller.locator("[data-timeline-virtual-content]")).toBeVisible()

  // The remount resumed the reading offset instead of opening at the top.
  await expect.poll(() => scroller.evaluate((element) => element.scrollTop)).toBeGreaterThan(0)

  // One signal covers both halves: only a kept older page AND a kept cursor can reach the
  // oldest page. A sync that reset either re-fetches the middle page and Prompt 11 never loads.
  await wheelUntilOlderPage(page, scroller, older, firstPage, async () => (await scroller.getByText(prompt(11)).count()) > 0)
  // The oldest page is only reachable by following the cursor the first page kept; a sync that reset
  // it would re-fetch the middle page instead. On the wire the query param is the mock's opaque token,
  // while `pageMessages` receives the decoded cursor token `"oldest"`.
  expect(older.some((url) => url.searchParams.has("cursor"))).toBe(true)
  expect(seenBefore).toContain("oldest")
  await expect(scroller.getByText(prompt(11))).toBeVisible({ timeout: 10_000 })

  // The newest page is still in the transcript after the sync.
  await scroller.evaluate((element) => {
    element.scrollTop = element.scrollHeight
  })
  await expect(scroller.getByText(prompt(35))).toBeVisible()
})

test("subagent peer messages stay visible in child history and transition from queued to sent on delivery", async ({
  page,
}) => {
  const childID = "ses_verify_queue_child"
  const queuedPeerID = "msg_vqd_peer_queued"
  const deliveredPeerMsg = {
    id: "msg_vqd_peer_sent",
    type: "synthetic" as const,
    time: { created: 1_015 },
    text: "Line one from user\nLine two detail",
    metadata: { source: "peer", from: "user" },
  }
  const queuedPeerInbox = {
    id: queuedPeerID,
    sessionID: childID,
    time: { created: 1_030 },
    type: "synthetic" as const,
    delivery: "queue" as const,
    payload: {
      text: "waiting peer instruction",
      metadata: { source: "peer", from: "user" },
    },
  }

  const prepared = await prepareSession(page, {
    sessionID: childID,
    title: "Verify subagent peer",
    sessions: [
      { id: sessionID, title: "Parent session" },
      { id: childID, parentID: sessionID, title: "Verify subagent peer" },
    ],
    pageMessages: (id) => {
      if (id !== childID) return { items: [] }
      return {
        items: [
          userMessage([textPart("prt_vqd_c0", "run child check")], { id: "msg_vqd_c_user", created: 1_000 }),
          assistantMessage([textPart("prt_vqd_c1", "working on child")], {
            id: "msg_vqd_c_asst",
            parentID: "msg_vqd_c_user",
            created: 1_010,
            completed: true,
          }),
          deliveredPeerMsg,
        ],
      }
    },
    sessionStatus: () => ({ [childID]: { type: "running" } }),
    inbox: () => [queuedPeerInbox],
  })

  const scroller = timelineScroller(page)
  await expect(scroller.locator("[data-timeline-virtual-content]")).toBeVisible()

  // Delivered peer message renders as a full multi-line peer notice in the child timeline.
  const sentNotice = scroller.locator('[data-slot="session-timeline-notice"][data-type="peer"]').first()
  await expect(sentNotice).toBeVisible()
  await expect(sentNotice).toContainText("Line one from user")
  await expect(sentNotice).toContainText("Line two detail")

  // While queued, the queued peer message stays in the child queue surfaces, not prematurely in timeline.
  await expect(page.getByTestId("session-peer-queued")).toContainText("1")
  await expect(page.locator('[data-component="session-queue-panel"]')).toContainText("waiting peer instruction")
  await expect(
    scroller.locator('[data-slot="session-timeline-notice"][data-type="peer"]', {
      hasText: "waiting peer instruction",
    }),
  ).toHaveCount(0)

  // Opening the peer drawer shows separate Queued (with Steer/Cancel actions) and Delivered history sections.
  await page.getByRole("button", { name: "Open peer messages" }).click()
  const drawer = page.locator('[data-component="session-peer-drawer"]')
  await expect(drawer).toBeVisible()
  await expect(drawer.locator('[data-peer-section="queued"]')).toContainText("waiting peer instruction")
  await expect(drawer.locator('[data-peer-section="queued"]').getByRole("button", { name: "Steer" })).toBeVisible()
  await expect(drawer.locator('[data-peer-section="queued"]').getByRole("button", { name: "Cancel" })).toBeVisible()
  await expect(drawer.locator('[data-peer-section="delivered"]')).toContainText("Line one from user")

  // Promoting delivery removes it from the queued section and reveals it chronologically in the timeline.
  await prepared.pushEvents([inboxDelivered({ sessionID: childID, inboxID: queuedPeerID })])
  await expect(drawer.locator('[data-peer-section="queued"]')).toHaveCount(0)
  await expect(drawer.locator('[data-peer-section="delivered"]')).toContainText("waiting peer instruction")
  await expect(
    scroller.locator('[data-slot="session-timeline-notice"][data-type="peer"]', {
      hasText: "waiting peer instruction",
    }),
  ).toHaveCount(1)
})
