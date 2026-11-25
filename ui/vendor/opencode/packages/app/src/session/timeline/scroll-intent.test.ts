import { expect, test } from "bun:test"
import { anchorCorrection, restorePlan, shouldLoadHistory } from "./virtualizer"

const ready = { loading: false, more: true, pinned: false, scrollTop: 0, bootstrapSpent: false }

/**
 * Mimics the real call path: every path (scroll event, fill, hash navigation, restore, anchor
 * correction) goes through the one gate before a page is requested. Counts actual loads.
 */
function harness() {
  let calls = 0

  return {
    get calls() {
      return calls
    },
    request(intent: Parameters<typeof shouldLoadHistory>[0]["intent"], overrides: Partial<typeof ready> = {}) {
      if (!shouldLoadHistory({ ...ready, intent, ...overrides })) return false
      calls += 1

      return true
    },
  }
}

test("a programmatic restore never pages history, even when it lands at the top", () => {
  const gate = harness()

  expect(gate.request("restore")).toBe(false)
  expect(gate.request("restore", { scrollTop: 0 })).toBe(false)
  expect(gate.calls).toBe(0)
})

test("an anchor/scroll correction never pages history and cannot start a prepend loop", () => {
  const gate = harness()

  // A remount correction writes scrollTop near the top repeatedly; none of it may load.
  for (let frame = 0; frame < 20; frame += 1) gate.request("correction", { scrollTop: frame })
  expect(gate.calls).toBe(0)

  // A cached mid-history restore that scrolls to the top is still a restore.
  const plan = restorePlan({ offset: 0, atEnd: false, anchor: { key: "user-1", top: 12 } }, false)

  expect(plan.restore).toBe(false)
  expect(gate.request("restore", { scrollTop: 0, pinned: false })).toBe(false)
  expect(gate.calls).toBe(0)
})

test("explicit user intent near the top pages older history", () => {
  const gate = harness()

  expect(gate.request("user", { scrollTop: 120 })).toBe(true)
  expect(gate.calls).toBe(1)
})

test("user intent while pinned or far from the top does not page", () => {
  const gate = harness()

  expect(gate.request("user", { pinned: true, scrollTop: 0 })).toBe(false)
  expect(gate.request("user", { scrollTop: 400 })).toBe(false)
  expect(gate.calls).toBe(0)
})

test("a remount/tab return that restored mid-history fetches nothing", () => {
  const gate = harness()
  const plan = restorePlan({ offset: 640, atEnd: false, anchor: { key: "user-1", top: 12 } }, false)

  expect(plan.restore).toBe(true)
  // The reattach observer keeps the restored offset instead of overwriting it with 0.
  expect(gate.request("restore", { scrollTop: 0 })).toBe(false)
  expect(gate.request("restore", { scrollTop: plan.offset })).toBe(false)
  expect(gate.calls).toBe(0)
})

test("an anchored correction that returns to the same row is a no-op, not a load", () => {
  const gate = harness()

  expect(anchorCorrection(12, 12)).toBe(0)
  expect(gate.request("correction", { scrollTop: 0 })).toBe(false)
  expect(gate.calls).toBe(0)
})

test("targeted hash navigation may page while pinned", () => {
  const gate = harness()

  expect(gate.request("navigation", { pinned: true })).toBe(true)
  expect(gate.calls).toBe(1)
})

test("the short-viewport bootstrap fills once and never refills", () => {
  const gate = harness()

  expect(gate.request("bootstrap", { pinned: true, bootstrapSpent: false })).toBe(true)
  expect(gate.request("bootstrap", { pinned: true, bootstrapSpent: true })).toBe(false)
  expect(gate.calls).toBe(1)
})

test("loading or exhausted history blocks every intent", () => {
  const gate = harness()

  expect(gate.request("user", { loading: true })).toBe(false)
  expect(gate.request("navigation", { more: false })).toBe(false)
  expect(gate.request("bootstrap", { more: false, pinned: true })).toBe(false)
  expect(gate.calls).toBe(0)
})

test("pinned mode follows new items without asking for older pages", () => {
  const gate = harness()

  // New content appends while pinned: the view stays pinned and no history intent exists.
  expect(gate.request("user", { pinned: true, scrollTop: 900 })).toBe(false)
  expect(gate.calls).toBe(0)
})
