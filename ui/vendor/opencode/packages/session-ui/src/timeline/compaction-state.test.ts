import { describe, expect, test } from "bun:test"
import {
  compactionFrom,
  compactionRowKey,
  emptyCompaction,
  foldCompaction,
  foldCompactionView,
  orderCompactionSignals,
  type CompactionSignal,
  type CompactionState,
} from "./compaction-state"

const phases = (signals: CompactionSignal[]) => compactionFrom(signals).phase

describe("foldCompaction", () => {
  test("suggested then queued then started then ended advances one record through every phase", () => {
    const signals: CompactionSignal[] = [
      { type: "suggested", at: 10, trigger: "observer", estimateTokens: 180_000, triggerTokens: 160_000 },
      { type: "queued", at: 20, inboxID: "inb_1", trigger: "observer" },
      { type: "started", at: 30, inboxID: "inb_1", trigger: "observer" },
      { type: "ended", at: 40, inboxID: "inb_1", trigger: "observer", beforeTokens: 180_000, afterTokens: 42_000 },
    ]

    // One record, one row identity per phase: walking the sequence never yields two records.
    const seen = signals.map((signal, index) => compactionFrom(signals.slice(0, index)).phase)
    expect(seen).toEqual(["none", "suggested", "queued", "running"])

    const done = compactionFrom(signals)
    expect(done).toEqual({
      phase: "done",
      inboxID: "inb_1",
      trigger: "observer",
      beforeTokens: 180_000,
      afterTokens: 42_000,
      estimateTokens: 180_000,
      triggerTokens: 160_000,
      startedAt: 30,
      endedAt: 40,
      at: 40,
    })
    expect(compactionRowKey(done)).toBe("compaction:done")
  })

  test("started without a prior queued row still opens a running compaction", () => {
    const state = compactionFrom([{ type: "started", at: 5, inboxID: "inb_9", trigger: "guard" }])
    expect(state).toEqual({ phase: "running", inboxID: "inb_9", trigger: "guard", startedAt: 5, at: 5 })
  })

  test("a delivery retires the queued badge", () => {
    const queued = compactionFrom([{ type: "queued", at: 1, inboxID: "inb_2", trigger: "manual" }])
    expect(queued.phase).toBe("queued")

    const delivered = foldCompaction(queued, { type: "delivered", at: 2, inboxID: "inb_2" })
    expect(delivered.phase).toBe("none")
    expect(delivered.inboxID).toBeUndefined()

    // A cancellation retires it the same way.
    expect(foldCompaction(queued, { type: "cancelled", at: 2, inboxID: "inb_2" }).phase).toBe("none")
    // A delivery for another inbox row leaves this one queued.
    expect(foldCompaction(queued, { type: "delivered", at: 2, inboxID: "other" })).toBe(queued)
  })

  test("a running or finished compaction is not retired by a delivery", () => {
    const running = compactionFrom([{ type: "started", at: 5, inboxID: "inb_3" }])
    expect(foldCompaction(running, { type: "delivered", at: 6, inboxID: "inb_3" })).toBe(running)
  })

  test("duplicate queued signals for the same inbox id produce one unchanged record", () => {
    const first = foldCompaction(emptyCompaction, { type: "queued", at: 7, inboxID: "inb_4", trigger: "guard" })
    const again = foldCompaction(first, { type: "queued", at: 7, inboxID: "inb_4", trigger: "guard" })
    const later = foldCompaction(again, { type: "queued", at: 7, inboxID: "inb_4" })

    expect(again).toBe(first)
    expect(later).toEqual(first)
    expect(compactionRowKey(again)).toBe(compactionRowKey(first))
  })

  test("a failed compaction keeps its error text", () => {
    const state = compactionFrom([
      { type: "started", at: 3, inboxID: "inb_5", trigger: "overflow" },
      { type: "failed", at: 4, inboxID: "inb_5", trigger: "overflow", error: "provider.context-length" },
    ])

    expect(state.phase).toBe("failed")
    expect(state.error).toBe("provider.context-length")
    expect(state.trigger).toBe("overflow")
    expect(state.startedAt).toBe(3)
    expect(state.endedAt).toBe(4)
  })

  test("folding the same sequence twice yields deep-equal records", () => {
    const signals: CompactionSignal[] = [
      { type: "started", at: 9, inboxID: "inb_6", trigger: "command" },
      { type: "suggested", at: 11, trigger: "observer", estimateTokens: 12_000 },
      { type: "ended", at: 12, inboxID: "inb_6", trigger: "command", beforeTokens: 12_000, afterTokens: 3_000 },
    ]

    expect(compactionFrom(signals)).toEqual(compactionFrom(signals))
    expect(compactionFrom([...signals].reverse())).toEqual(compactionFrom(signals))
  })

  test("an event that overtakes a snapshot cannot move the phase backwards", () => {
    const ended = compactionFrom([{ type: "ended", at: 30, inboxID: "inb_7", trigger: "manual" }])
    const lateQueued = foldCompaction(ended, { type: "queued", at: 10, inboxID: "inb_7" })

    expect(lateQueued.phase).toBe("done")
    expect(lateQueued).toBe(ended)
  })

  test("a suggestion is superseded by queueing and a stale one cannot bring the row back", () => {
    const suggested = compactionFrom([{ type: "suggested", at: 1, estimateTokens: 150_000 }])
    expect(suggested.phase).toBe("suggested")

    const queued = foldCompaction(suggested, { type: "queued", at: 2, inboxID: "inb_8" })
    expect(queued.phase).toBe("queued")
    // The advisory is gone the moment the compaction is queued, and an older suggestion cannot restore it.
    expect(foldCompaction(queued, { type: "suggested", at: 1, estimateTokens: 150_000 })).toBe(queued)

    const running = foldCompaction(queued, { type: "started", at: 3, inboxID: "inb_8" })
    expect(foldCompaction(running, { type: "suggested", at: 4 }).phase).toBe("running")

    const afterEnd = foldCompaction(running, { type: "ended", at: 5, inboxID: "inb_8" })
    expect(afterEnd.phase).toBe("done")
    // The finished compaction keeps its row; only a newer observation opens the next advisory.
    expect(foldCompaction(afterEnd, { type: "suggested", at: 6, estimateTokens: 190_000 })).toMatchObject({
      phase: "suggested",
      at: 6,
      estimateTokens: 190_000,
    })
  })

  test("a new compaction does not inherit the finished one's measurements", () => {
    const done = compactionFrom([
      { type: "started", at: 1, inboxID: "inb_old", trigger: "guard" },
      { type: "ended", at: 2, inboxID: "inb_old", trigger: "guard", beforeTokens: 200_000, afterTokens: 30_000 },
    ])
    const next = foldCompaction(done, { type: "queued", at: 3, inboxID: "inb_new" })

    expect(next.phase).toBe("queued")
    expect(next.inboxID).toBe("inb_new")
    expect(next.beforeTokens).toBeUndefined()
    expect(next.afterTokens).toBeUndefined()
    expect(next.trigger).toBeUndefined()
  })

  test("orders signals deterministically, ties broken by stage", () => {
    const queued: CompactionSignal = { type: "queued", at: 5, inboxID: "inb_a" }
    const started: CompactionSignal = { type: "started", at: 5, inboxID: "inb_a" }
    expect(orderCompactionSignals([started, queued])).toEqual([queued, started])
    // Sorting is stable regardless of the input order.
    expect(orderCompactionSignals([started, queued])).toEqual(orderCompactionSignals([queued, started]))
  })

  test("a durable view folds a pending row and a running message into one running record", () => {
    const state = foldCompactionView(emptyCompaction, {
      pending: [{ id: "inb_v", type: "compaction", time: { created: 10 } }],
      messages: [{ id: "msg_v", type: "compaction", status: "running", time: { created: 12 } }],
    })

    expect(state.phase).toBe("running")
    // The running message is the later signal, so it owns the inbox id.
    expect(state.inboxID).toBe("msg_v")
    expect(state.startedAt).toBe(12)
  })

  test("a reloaded session shows a finished compaction from its durable message", () => {
    const state = foldCompactionView(
      emptyCompaction,
      { messages: [{ id: "msg_done", type: "compaction", status: "completed", time: { created: 20 } }] },
      [],
    )

    expect(state.phase).toBe("done")
    expect(state.endedAt).toBe(20)
  })

  test("an empty view is the empty record", () => {
    expect(foldCompactionView(emptyCompaction, {}, [])).toEqual(emptyCompaction)
    expect(compactionFrom([])).toEqual(emptyCompaction)
    expect(compactionRowKey(emptyCompaction)).toBe("compaction:none")
  })
})

describe("CompactionState shape", () => {
  test("every optional field is omitted rather than null", () => {
    const state: CompactionState = compactionFrom([{ type: "queued", at: 1, inboxID: "inb_x" }])
    expect(Object.values(state).some((value) => value === null)).toBe(false)
    expect(Object.keys(state).sort()).toEqual(["at", "inboxID", "phase"])
  })
})
