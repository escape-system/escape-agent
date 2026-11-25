import { expect, test } from "@playwright/test"
import { assistantMessage, inboxEnqueued, textPart, userMessage } from "../utils/timeline"
import { expectNoHorizontalOverflow, prepareSession, timelineScroller } from "./agentic-stubs"

// Verification-only mobile specs (DRAFTED during the reserved phase, NOT executed).
//
// Covers the ui-wave-contract mobile clauses that currently have NO evidence:
//   - tab away/back restores the reading offset instead of jumping to the top;
//   - queue strip + subagent entry fit 320 and 390 px with no horizontal overflow;
//   - the composer stays on screen when the viewport shrinks (keyboard surrogate);
//   - the agent drawer opens, shows the tree, and returns focus to its trigger;
//   - 44 px touch targets (expected RED until the queue strip is resized; kept as
//     its own test so the failure is attributable to that clause alone).
//
// Limits stated openly: Chromium headless cannot raise a real software keyboard
// and applies no real safe-area insets, so "keyboard" is emulated by shrinking
// the viewport height and safe-area padding is only indirectly exercised. Real
// device proof remains missing.

const sessionID = "ses_verify_mobile"
const childID = "ses_verify_mobile_child"

const longText = Array.from({ length: 30 }, (_, index) => `Reading block ${index}.`).join("\n\n")

function history() {
  return Array.from({ length: 12 }, (_, index) =>
    index % 2 === 0
      ? userMessage([textPart(`prt_mob_user_${index}`, `Prompt ${index}`)], {
          id: `msg_mob_user_${index}`,
          created: 1_000 + index * 10,
        })
      : assistantMessage([textPart(`prt_mob_assistant_${index}`, longText)], {
          id: `msg_mob_assistant_${index}`,
          parentID: `msg_mob_user_${index - 1}`,
          created: 1_000 + index * 10,
          completed: true,
        }),
  )
}

function tree(id: string) {
  return {
    root: id,
    nodes: [
      { id, agent: "build", state: "busy", depth: 0, role: "primary", spawnAllowed: true },
      { id: childID, parentID: id, agent: "explore", state: "idle", depth: 1, role: "subagent", spawnAllowed: false },
    ],
  }
}

async function open(page: import("@playwright/test").Page, width: number) {
  return prepareSession(page, {
    sessionID,
    title: "Verify mobile",
    sessions: [
      { id: sessionID, title: "Verify mobile" },
      { id: childID, parentID: sessionID, title: "Verify mobile child" },
    ],
    pageMessages: () => ({ items: history() }),
    // Busy so queued prompts wait behind a turn instead of steering.
    sessionStatus: () => ({ [sessionID]: { type: "running" } }),
    tree,
    width,
    height: 844,
  })
}

/** A live queued inbox row for this session. */
function queued(inboxID: string, text: string) {
  return inboxEnqueued({
    sessionID,
    inboxID,
    item: { type: "user", payload: { text }, delivery: "queue" },
  })
}

test.describe("mobile session", () => {
  test.use({
    viewport: { width: 390, height: 844 },
    hasTouch: true,
    isMobile: true,
    serviceWorkers: "block",
  })

  test("switching views and back restores the reading offset", async ({ page }) => {
    await open(page, 390)

    const scroller = timelineScroller(page)
    await expect(scroller.locator("[data-timeline-virtual-content]")).toBeVisible()

    // Unpin by scrolling up, then sit at a mid-history offset (not 0, not the end). A real wheel
    // carries the intent; a synthetic WheelEvent scrolls nothing and leaves the intent armed instead.
    await scroller.hover()
    await page.mouse.wheel(0, -600)
    await expect.poll(() => scroller.evaluate((element) => element.scrollTop)).toBeGreaterThan(300)
    const offset = await scroller.evaluate((element) => element.scrollTop)
    expect(offset).toBeGreaterThan(300)

    // On mobile, selecting a non-session view unmounts the timeline; returning remounts it.
    const tabs = page.getByRole("tablist", { name: "Session view", exact: true })
    await tabs.getByRole("tab", { name: "Changes", exact: true }).click()
    await expect(scroller).toHaveCount(0)
    await tabs.getByRole("tab", { name: "Session", exact: true }).click()
    await expect(scroller.locator("[data-timeline-virtual-content]")).toBeVisible()

    // The regression this guards: a fresh root starts at 0 and throws the reader to the oldest row.
    await expect.poll(() => scroller.evaluate((element) => element.scrollTop)).toBeGreaterThan(300)
    const restored = await scroller.evaluate((element) => element.scrollTop)
    expect(Math.abs(restored - offset)).toBeLessThanOrEqual(200)
  })

  for (const width of [320, 390] as const) {
    test(`queue and subagent entries fit ${width}px and the composer survives a keyboard shrink`, async ({ page }) => {
      const prepared = await open(page, width)
      await prepared.pushEvents([queued("msg_verify_mobile_queue", "queued on mobile")])

      const strip = page.locator('[data-component="session-queue-panel"]')
      await expect(strip).toBeVisible()
      await expect(strip.getByText("queued on mobile", { exact: true })).toBeVisible()
      await expectNoHorizontalOverflow(page)

      const editor = page.getByRole("textbox", { name: "Prompt", exact: true })
      await expect(editor).toBeVisible()
      const trigger = page.locator('[data-component="session-agent-panels-mobile-trigger"]')
      await expect(trigger).toBeVisible()
      await expect(trigger).toHaveAttribute("aria-haspopup", "dialog")

      // Keyboard surrogate: ~450 px of vertical space disappears, as when a soft keyboard opens.
      await page.setViewportSize({ width, height: 400 })
      const box = await editor.evaluate((element) => ({
        top: element.getBoundingClientRect().top,
        bottom: element.getBoundingClientRect().bottom,
        viewport: window.innerHeight,
      }))
      expect(box.bottom).toBeLessThanOrEqual(box.viewport + 1)
      expect(box.top).toBeGreaterThanOrEqual(0)
      await expectNoHorizontalOverflow(page)
    })
  }

  test("the agent drawer opens on the tree and returns focus to its trigger", async ({ page }) => {
    await open(page, 390)

    const trigger = page.locator('[data-component="session-agent-panels-mobile-trigger"]')
    await expect(trigger).toBeVisible()
    await trigger.focus()
    await trigger.press("Enter")

    const drawer = page.locator('[data-slot="mobile-panel"]')
    await expect(drawer).toBeVisible()
    await expect(drawer.locator('[data-component="session-agent-tree"]')).toBeVisible()
    await expectNoHorizontalOverflow(page)

    await page.keyboard.press("Escape")
    await expect(drawer).toBeHidden()
    // A sheet that loses the opener's focus breaks keyboard and switch users.
    await expect(trigger).toBeFocused()
  })

  test("queue rows and the agent trigger meet the 44px touch-target contract", async ({ page }) => {
    const prepared = await open(page, 390)
    await prepared.pushEvents([queued("msg_verify_mobile_targets", "touch target")])

    const row = page.locator('[data-component="session-queue-row"]').first()
    await expect(row).toBeVisible()
    // Wait for Grow spring animation to settle to the row's full 44px touch height.
    await expect.poll(async () => (await row.boundingBox())?.height ?? 0).toBeGreaterThanOrEqual(44)

    const steer = row.locator('[data-action="session-queue-steer"]')
    const remove = row.locator('[data-action="session-queue-remove"]')
    await expect(steer).toBeVisible()
    await expect(remove).toBeVisible()
    await expect.poll(async () => (await steer.boundingBox())?.height ?? 0).toBeGreaterThanOrEqual(44)
    await expect.poll(async () => (await remove.boundingBox())?.height ?? 0).toBeGreaterThanOrEqual(44)
    await expect.poll(async () => (await remove.boundingBox())?.width ?? 0).toBeGreaterThanOrEqual(44)

    const trigger = page.locator('[data-component="session-agent-panels-mobile-trigger"]')
    const box = await trigger.boundingBox()
    expect(box?.height ?? 0).toBeGreaterThanOrEqual(44)
    expect(box?.width ?? 0).toBeGreaterThanOrEqual(44)
  })
})
