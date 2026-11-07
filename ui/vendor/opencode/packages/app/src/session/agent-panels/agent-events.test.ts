import { describe, expect, test } from "bun:test"
import { createRoot } from "solid-js"

type RawEvent = { type: string; data?: unknown }
type Handler = (event: RawEvent) => void

/** The raw SDK surface `createAgentEvents` and `createPeerFeed` read. */
function fakeServer() {
  const handlers = new Set<Handler>()

  return {
    sdk: {
      scope: "local",
      event: {
        listen(handler: Handler) {
          handlers.add(handler)

          return () => handlers.delete(handler)
        },
      },
    },
    emit(type: string, data?: unknown) {
      for (const handler of [...handlers]) handler({ type, data })
    },
    listeners: () => handlers.size,
  }
}

let server = fakeServer()

// The panels only need the server's event bus; the typed clients are not part of these paths.
const { mock } = await import("bun:test")

mock.module("@/runtime/server/client", () => ({ useServerSDK: () => server.sdk }))

const { createAgentEvents, createPeerFeed } = await import("./agent-events")

function events(sessionID: () => string | undefined, root: () => string | undefined) {
  server = fakeServer()
  return createRoot((dispose) => ({ ...createAgentEvents({ sessionID, root }), dispose }))
}

describe("agent events", () => {
  const rootID = "ses_root"
  const sessionID = "ses_child"

  test("keeps a sibling tree's repairs off this session", () => {
    const panel = events(() => sessionID, () => rootID)
    try {
      server.emit("observer.repaired", { rootID: "ses_sibling", kind: "tool", reason: "sibling", attempt: 1 })
      expect(panel.repairs()).toEqual([])
      expect(panel.escalation()).toBeUndefined()

      server.emit("observer.repaired", { rootID, kind: "tool", reason: "ours", target: "call_1", attempt: 1 })
      expect(panel.repairs().map((repair) => repair.reason)).toEqual(["ours"])
    } finally {
      panel.dispose()
    }
  })

  test("matches repairs addressed to the session itself and prepends newest first", () => {
    const panel = events(() => sessionID, () => rootID)
    try {
      server.emit("observer.repaired", { rootID: sessionID, kind: "tool", reason: "first", attempt: 1 })
      server.emit("observer.repaired", { rootID, kind: "tool", reason: "second", attempt: 2 })
      expect(panel.repairs().map((repair) => repair.reason)).toEqual(["second", "first"])
    } finally {
      panel.dispose()
    }
  })

  test("caps the repair feed at twenty rows", () => {
    const panel = events(() => sessionID, () => rootID)
    try {
      for (let index = 0; index < 25; index++) {
        server.emit("observer.repaired", { rootID, kind: "tool", reason: `repair ${index}`, attempt: index })
      }
      const repairs = panel.repairs()
      expect(repairs).toHaveLength(20)
      expect(repairs[0]?.reason).toBe("repair 24")
      expect(repairs[19]?.reason).toBe("repair 5")
    } finally {
      panel.dispose()
    }
  })

  test("drops unreadable repair payloads without clearing an existing feed", () => {
    const panel = events(() => sessionID, () => rootID)
    try {
      server.emit("observer.repaired", { rootID, kind: "tool", reason: "kept" })
      server.emit("observer.repaired", { rootID })
      server.emit("observer.repaired", { kind: "tool", reason: "no root" })
      server.emit("observer.repaired")
      expect(panel.repairs().map((repair) => repair.reason)).toEqual(["kept"])
    } finally {
      panel.dispose()
    }
  })

  test("sets the escalation reason and budget for this tree", () => {
    const panel = events(() => sessionID, () => rootID)
    try {
      server.emit("observer.escalated", { rootID: "ses_sibling", reason: "sibling", budget: 9 })
      expect(panel.escalation()).toBeUndefined()

      server.emit("observer.escalated", { rootID, reason: "budget exhausted", budget: 3 })
      expect(panel.escalation()).toEqual({ reason: "budget exhausted", budget: 3 })

      server.emit("observer.escalated", { rootID })
      expect(panel.escalation()).toEqual({ reason: "", budget: undefined })
    } finally {
      panel.dispose()
    }
  })

  test("adopts goal.updated only for this tree", () => {
    const panel = events(() => sessionID, () => rootID)
    try {
      server.emit("goal.updated", { rootID: "ses_sibling", goal: { id: "gol_other", objective: "other" } })
      expect(panel.goal()).toBeUndefined()

      server.emit("goal.updated", { rootID, goal: { id: "gol_1", objective: "ship", rounds: 2 } })
      expect(panel.goal()).toMatchObject({ id: "gol_1", objective: "ship", rounds: 2, status: "active" })

      server.emit("goal.updated", { rootID, goal: { objective: "unreadable" } })
      expect(panel.goal()?.id).toBe("gol_1")
    } finally {
      panel.dispose()
    }
  })

  // `activeTickets` is a `createMemo`, and the server build of Solid evaluates memos once. Its filtering is
  // covered under the browser build in `test-browser/agent-events.test.ts`.

  test("stops listening when its owner is disposed", () => {
    const panel = events(() => sessionID, () => rootID)
    server.emit("observer.repaired", { rootID, kind: "tool", reason: "before" })
    expect(panel.repairs()).toHaveLength(1)

    panel.dispose()
    expect(server.listeners()).toBe(0)
    server.emit("observer.repaired", { rootID, kind: "tool", reason: "after" })
    expect(panel.repairs()).toHaveLength(1)
  })
})

describe("peer feed", () => {
  test("folds duplicate ids into one row and increments unread per arrival", () => {
    const feed = createRoot((dispose) => ({ ...createPeerFeed(() => "ses_child"), dispose }))
    try {
      const message = { id: "row_1", from: "ses_peer", text: "hello", created: 1 }
      feed.add(message)
      feed.add({ ...message, text: "changed" })
      expect(feed.messages()).toEqual([message])
      expect(feed.unread()).toBe(2)
    } finally {
      feed.dispose()
    }
  })

  test("marks every arrival read on demand", () => {
    const feed = createRoot((dispose) => ({ ...createPeerFeed(() => "ses_child"), dispose }))
    try {
      feed.add({ id: "row_1", from: "ses_peer", text: "one", created: 1 })
      feed.add({ id: "row_2", from: "ses_peer", text: "two", created: 2 })
      expect(feed.unread()).toBe(2)
      feed.markRead()
      expect(feed.unread()).toBe(0)
      expect(feed.messages().map((message) => message.id)).toEqual(["row_1", "row_2"])
    } finally {
      feed.dispose()
    }
  })

  test("accepts only events addressed to its own session", () => {
    server = fakeServer()
    const feed = createRoot((dispose) => ({ ...createPeerFeed(() => "ses_child"), dispose }))
    try {
      server.emit("subagent.peer.message", { sessionID: "ses_other", text: "not mine", created: 1 })
      server.emit("peer.message", { sessionID: "ses_child", text: "wrong type", created: 2 })
      server.emit("subagent.peer.message", { sessionID: "ses_child", text: "", created: 3 })
      expect(feed.messages()).toEqual([])
      expect(feed.unread()).toBe(0)

      server.emit("subagent.peer.message", { sessionID: "ses_child", from: "ses_peer", text: "mine", created: 4 })
      expect(feed.messages()).toHaveLength(1)
      expect(feed.messages()[0]).toMatchObject({ from: "ses_peer", text: "mine", created: 4 })
      expect(feed.unread()).toBe(1)
    } finally {
      feed.dispose()
    }
  })

  test("stops listening when its drawer closes", () => {
    server = fakeServer()
    const feed = createRoot((dispose) => ({ ...createPeerFeed(() => "ses_child"), dispose }))
    feed.dispose()
    expect(server.listeners()).toBe(0)

    server.emit("subagent.peer.message", { sessionID: "ses_child", text: "late", created: 1 })
    expect(feed.messages()).toEqual([])
  })
})
