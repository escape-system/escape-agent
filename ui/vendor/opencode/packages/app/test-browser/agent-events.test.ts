import { describe, expect, mock, test } from "bun:test"
import { createRoot } from "solid-js"

// Runs under the browser build: `activeTickets` is a memo that the server build evaluates once.

type RawEvent = { type: string; data?: unknown }
type Handler = (event: RawEvent) => void

const listeners = new Set<Handler>()

mock.module("@/runtime/server/client", () => ({
  useServerSDK: () => ({
    scope: "local",
    event: {
      listen(handler: Handler) {
        listeners.add(handler)

        return () => listeners.delete(handler)
      },
    },
  }),
}))

const { createAgentEvents } = await import("@/session/agent-panels/agent-events")

const emit = (type: string, data?: unknown) => {
  for (const listener of [...listeners]) listener({ type, data })
}

describe("active lane tickets", () => {
  test("drops terminal states when their ticket event arrives", () => {
    const panel = createRoot((dispose) => ({
      ...createAgentEvents({ sessionID: () => "ses_child", root: () => "ses_root" }),
      dispose,
    }))

    try {
      emit("lane.queued", { lane: "cargo", ticket: { id: "tkt_1", state: "queued" }, sessionID: "ses_child" })
      emit("lane.started", { lane: "cargo", ticket: { id: "tkt_2", state: "running" }, sessionID: "ses_child" })
      emit("lane.queued", { lane: "cargo", ticket: { id: "tkt_3", state: "queued" }, sessionID: "ses_other" })
      emit("lane.queued", { lane: "cargo", ticket: { id: "tkt_4", state: "queued" }, sessionID: "ses_root" })
      expect(panel.activeTickets().map((ticket) => ticket.id)).toEqual(["tkt_1", "tkt_2"])

      emit("lane.finished", { lane: "cargo", ticket: { id: "tkt_1", state: "succeeded" }, sessionID: "ses_child" })
      emit("lane.cancelled", { lane: "cargo", ticket: { id: "tkt_2", state: "cancelled" }, sessionID: "ses_child" })
      expect(panel.activeTickets()).toEqual([])

      emit("lane.queued", { lane: "cargo", ticket: { id: "tkt_5", state: "queued" }, sessionID: "ses_child" })
      expect(panel.activeTickets().map((ticket) => ticket.id)).toEqual(["tkt_5"])
    } finally {
      panel.dispose()
    }
  })

  test("ignores an envelope that names another session even when the ticket names this one", () => {
    const panel = createRoot((dispose) => ({
      ...createAgentEvents({ sessionID: () => "ses_child", root: () => "ses_root" }),
      dispose,
    }))

    try {
      emit("lane.queued", {
        lane: "cargo",
        ticket: { id: "tkt_1", state: "queued", ownerSession: "ses_child" },
        sessionID: "ses_other",
      })
      expect(panel.activeTickets()).toEqual([])
    } finally {
      panel.dispose()
    }
  })
})
