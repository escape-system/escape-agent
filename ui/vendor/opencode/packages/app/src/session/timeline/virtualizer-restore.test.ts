import { expect, test } from "bun:test"
import {
  anchorCorrection,
  clampRestoredOffset,
  prependAnchorDelta,
  reconnectRestore,
  restorePlan,
  restoredOffsetReport,
} from "./virtualizer"

const cachedMidHistory = {
  offset: 640,
  atEnd: false,
  anchor: { key: "user-1", top: 12 },
}

test("an unpinned remount resumes the cached offset instead of the oldest row", () => {
  expect(restorePlan(cachedMidHistory, false)).toEqual({ offset: 640, restore: true })
})

test("a pinned remount still follows the end and ignores the cached offset", () => {
  expect(restorePlan(cachedMidHistory, true)).toEqual({ offset: Number.MAX_SAFE_INTEGER, restore: false })
  expect(restorePlan(undefined, true).offset).toBe(Number.MAX_SAFE_INTEGER)
})

test("no cache, an at-end view, or a zero offset keeps the cold start", () => {
  expect(restorePlan(undefined, false)).toEqual({ offset: 0, restore: false })
  expect(restorePlan({ offset: 640, atEnd: true, anchor: undefined }, false)).toEqual({ offset: 0, restore: false })
  expect(restorePlan({ offset: 0, atEnd: false, anchor: cachedMidHistory.anchor }, false)).toEqual({
    offset: 0,
    restore: false,
  })
})

test("a restored offset is clamped to the freshly bound scroll range", () => {
  expect(clampRestoredOffset(640, 2_000, 800)).toBe(640)
  expect(clampRestoredOffset(640, 900, 800)).toBe(100)
  expect(clampRestoredOffset(640, 700, 800)).toBe(0)
  expect(clampRestoredOffset(-5, 2_000, 800)).toBe(0)
})

test("the first-visible anchor corrects scrollTop by the row's drift", () => {
  // The row painted 30px lower than at capture, so the view moves down by 30 to put it back.
  expect(anchorCorrection(12, 42)).toBe(30)
  expect(anchorCorrection(12, -18)).toBe(-30)
  // Sub-pixel drift is not worth a write.
  expect(anchorCorrection(12, 12.4)).toBe(0)
  expect(anchorCorrection(12, 12)).toBe(0)
})

test("a zero report from a reattached element does not overwrite the restored offset", () => {
  expect(restoredOffsetReport(true, 0)).toEqual({ keep: true, pending: true })
  expect(restoredOffsetReport(true, 640)).toEqual({ keep: false, pending: false })
  expect(restoredOffsetReport(false, 0)).toEqual({ keep: false, pending: false })
})

test("a reattached view puts an unpinned reader back at the offset they left", () => {
  // A mobile view switch detaches and reinserts the same element, which reads 0 again.
  expect(reconnectRestore({ pinned: false, last: 640, live: 0, scrollHeight: 4_000, clientHeight: 800 })).toBe(640)
  // The row heights settled differently, so the cached offset is clamped into the new range.
  expect(reconnectRestore({ pinned: false, last: 3_600, live: 0, scrollHeight: 4_000, clientHeight: 800 })).toBe(3_200)
})

test("a reattached view keeps following its end instead of restoring", () => {
  expect(reconnectRestore({ pinned: true, last: 640, live: 0, scrollHeight: 4_000, clientHeight: 800 })).toBeUndefined()
})

test("a reattached view writes nothing when there is nothing to recover", () => {
  // A first mount, or a reader who was already at the top.
  expect(reconnectRestore({ pinned: false, last: 0, live: 0, scrollHeight: 4_000, clientHeight: 800 })).toBeUndefined()
  // Chromium kept the offset across the reinsertion.
  expect(
    reconnectRestore({ pinned: false, last: 640, live: 640, scrollHeight: 4_000, clientHeight: 800 }),
  ).toBeUndefined()
  // A range too short to hold the cached offset would only jump to the top.
  expect(reconnectRestore({ pinned: false, last: 640, live: 0, scrollHeight: 700, clientHeight: 800 })).toBeUndefined()
})

test("prependAnchorDelta shifts past newly prepended rows without triggering on appends or session switches", () => {
  const sizes = new Map<string, number>([
    ["user-10", 300],
    ["user-11", 240],
  ])
  expect(
    prependAnchorDelta(
      ["user-12", "assistant-12"],
      ["user-10", "user-11", "user-12", "assistant-12"],
      (key) => sizes.get(key) ?? 60,
    ),
  ).toEqual({
    shift: 2,
    prepended: ["user-10", "user-11"],
    delta: 540,
  })

  expect(
    prependAnchorDelta(
      ["user-12", "assistant-12"],
      ["user-12", "assistant-12", "user-13"],
      (key) => sizes.get(key) ?? 60,
    ),
  ).toEqual({ shift: 0, prepended: [], delta: 0 })

  expect(
    prependAnchorDelta(
      ["user-12", "assistant-12"],
      ["user-99", "assistant-99", "user-98"],
      (key) => sizes.get(key) ?? 60,
    ),
  ).toEqual({ shift: 0, prepended: [], delta: 0 })
})
