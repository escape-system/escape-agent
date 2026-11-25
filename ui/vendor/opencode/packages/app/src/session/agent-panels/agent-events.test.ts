import { describe, expect, test } from "bun:test"
import { createRoot } from "solid-js"
import { createStore } from "solid-js/store"

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

/**
 * The store slice the peer feed reads: `pending.list` and `message.list`. The
 * real store is a `createStore` inside `useServer()`; here it is the same shape
 * so a new mount reads the same snapshot and cannot duplicate rows.
 */
function fakeStore() {
  const [store, setStore] = createStore<{
    session: { pending: Record<string, unknown[]>; message: Record<string, unknown[]> }
  }>({ session: { pending: {}, message: {} } })

  return {
    store: {
      session: {
        pending: { list: (sessionID: string) => store.session.pending[sessionID] ?? [] },
        message: { list: (sessionID: string) => store.session.message[sessionID] ?? [] },
      },
    },
    setPending(sessionID: string, rows: unknown[]) {
      setStore("session", "pending", sessionID, rows)
    },
    setMessages(sessionID: string, rows: unknown[]) {
      setStore("session", "message", sessionID, rows)
    },
  }
}

let server = fakeServer()
let state = fakeStore()

// The panels only need the server's event bus and the store accessors; the typed clients are not part of these paths.
const { mock } = await import("bun:test")

mock.module("@/runtime/server/client", () => ({ useServerSDK: () => server.sdk }))
mock.module("@/runtime/server/current", () => ({
  useServer: () => ({ ctx: { data: state.store } }),
}))

const { createAgentEvents, createPeerFeed } = await import("./agent-events")

function events(sessionID: () => string | undefined, root: () => string | undefined) {
  server = fakeServer()
  return createRoot((dispose) => ({ ...createAgentEvents({ sessionID, root }), dispose }))
}

function feed(sessionID: () => string | undefined = () => "ses_child") {
  return createRoot((dispose) => ({ ...createPeerFeed(sessionID), dispose }))
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
  const inbox = (id: string, text: string, created: number, extra: Record<string, unknown> = {}) => ({
    id,
    sessionID: "ses_child",
    time: { created },
    type: "synthetic",
    delivery: "queue",
    payload: { text, peer: true, metadata: { source: "peer", from: "user" }, ...extra },
  })

  const delivered = (id: string, text: string, created: number, extra: Record<string, unknown> = {}) => ({
    id,
    sessionID: "ses_child",
    time: { created },
    type: "synthetic",
    text,
    peer: true,
    metadata: { source: "peer", from: "user" },
    ...extra,
  })

  test("hydrates delivered peer rows from the transcript", () => {
    state = fakeStore()
    state.setMessages("ses_child", [
      delivered("msg_1", "hello", 1),
      { id: "msg_2", sessionID: "ses_child", time: { created: 2 }, type: "user", text: "plain user" },
      delivered("msg_3", "legacy", 3, { type: "peer" }),
    ])
    const peer = feed()
    try {
      expect(peer.messages().map((row) => row.id)).toEqual(["msg_1", "msg_3"])
      expect(peer.messages().every((row) => row.status === "sent")).toBe(true)
      // Hydration is history, not an arrival.
      expect(peer.unread()).toBe(0)
    } finally {
      peer.dispose()
    }
  })

  test("hydrates queued rows from the inbox and keeps them out of the delivered list", () => {
    state = fakeStore()
    state.setPending("ses_child", [
      inbox("msg_q1", "queued", 4),
      { ...inbox("msg_q2", "user prompt", 5), payload: { text: "user prompt" } },
      { ...inbox("msg_q3", "compaction", 6), type: "compaction", payload: {} },
    ])
    const peer = feed()
    try {
      expect(peer.queued().map((row) => row.id)).toEqual(["msg_q1"])
      expect(peer.messages().map((row) => row.status)).toEqual(["queued"])
      expect(peer.unread()).toBe(0)
    } finally {
      peer.dispose()
    }
  })

  test("dedupes a live event against the inbox row it names", () => {
    state = fakeStore()
    state.setPending("ses_child", [inbox("msg_q1", "queued", 4)])
    const peer = feed()
    try {
      server.emit("subagent.peer.message", {
        sessionID: "ses_child",
        from: "user",
        text: "queued",
        created: 4,
        messageID: "msg_q1",
      })
      expect(peer.messages()).toHaveLength(1)
      expect(peer.queued()).toHaveLength(1)

      // While still in inbox, transcript materialization keeps status `"queued"`.
      state.setMessages("ses_child", [delivered("msg_q1", "queued", 4)])
      expect(peer.messages()).toHaveLength(1)
      expect(peer.messages()[0]?.status).toBe("queued")

      // Delivery removes it from inbox and promotes it cleanly to `"sent"`.
      server.emit("session.inbox.delivered", { sessionID: "ses_child", inboxID: "msg_q1" })
      state.setPending("ses_child", [])
      expect(peer.messages()).toHaveLength(1)
      expect(peer.messages()[0]?.status).toBe("sent")
    } finally {
      peer.dispose()
    }
  })

  test("reopening against the same store snapshot does not duplicate rows or inflate unread", () => {
    state = fakeStore()
    state.setMessages("ses_child", [delivered("msg_1", "hello", 1)])
    state.setPending("ses_child", [inbox("msg_q1", "queued", 2)])

    const first = feed()
    expect(first.messages().map((row) => row.id)).toEqual(["msg_1", "msg_q1"])
    first.dispose()

    server = fakeServer()
    const second = feed()
    try {
      expect(second.messages().map((row) => row.id)).toEqual(["msg_1", "msg_q1"])
      expect(second.unread()).toBe(0)
    } finally {
      second.dispose()
    }
  })

  test("counts only live arrivals as unread and marks them read on demand", () => {
    state = fakeStore()
    state.setMessages("ses_child", [delivered("msg_1", "history", 1)])
    const peer = feed()
    try {
      expect(peer.unread()).toBe(0)
      server.emit("subagent.peer.message", { sessionID: "ses_child", from: "ses_peer", text: "live", created: 9 })
      expect(peer.messages().map((row) => row.text)).toEqual(["history", "live"])
      expect(peer.unread()).toBe(1)
      peer.markRead()
      expect(peer.unread()).toBe(0)
    } finally {
      peer.dispose()
    }
  })

  test("does not re-count a re-emitted live event id", () => {
    state = fakeStore()
    const peer = feed()
    try {
      const event = { sessionID: "ses_child", from: "ses_peer", text: "hi", created: 3, messageID: "msg_9" }
      server.emit("subagent.peer.message", event)
      server.emit("subagent.peer.message", event)
      expect(peer.messages()).toHaveLength(1)
      expect(peer.unread()).toBe(1)
    } finally {
      peer.dispose()
    }
  })

  test("accepts only events addressed to its own session", () => {
    state = fakeStore()
    server = fakeServer()
    const peer = feed()
    try {
      server.emit("subagent.peer.message", { sessionID: "ses_other", text: "not mine", created: 1 })
      server.emit("peer.message", { sessionID: "ses_child", text: "wrong type", created: 2 })
      server.emit("subagent.peer.message", { sessionID: "ses_child", text: "", created: 3 })
      expect(peer.messages()).toEqual([])
      expect(peer.unread()).toBe(0)

      server.emit("subagent.peer.message", { sessionID: "ses_child", from: "ses_peer", text: "mine", created: 4 })
      expect(peer.messages()).toHaveLength(1)
      expect(peer.messages()[0]).toMatchObject({ from: "ses_peer", text: "mine", created: 4 })
      expect(peer.unread()).toBe(1)
    } finally {
      peer.dispose()
    }
  })

  test("stops listening when its drawer closes", () => {
    state = fakeStore()
    server = fakeServer()
    const peer = feed()
    peer.dispose()
    expect(server.listeners()).toBe(0)

    server.emit("subagent.peer.message", { sessionID: "ses_child", text: "late", created: 1 })
    expect(peer.messages()).toEqual([])
  })

  test("keeps queued status on active inbox rows even when materialized in transcript, until delivered", () => {
    state = fakeStore()
    server = fakeServer()
    const peer = feed()
    try {
      const id = "msg_steer_mat"
      const payload = { text: "steer instruction", peer: true, metadata: { source: "peer", from: "user" } }
      state.setMessages("ses_child", [
        {
          id,
          type: "synthetic",
          text: "steer instruction",
          peer: true,
          metadata: { source: "peer", from: "user" },
          time: { created: 10 },
        },
      ])
      state.setPending("ses_child", [
        { id, type: "synthetic", delivery: "steer", payload, time: { created: 10 } },
      ])

      expect(peer.messages()).toHaveLength(1)
      expect(peer.messages()[0]).toMatchObject({ id, status: "queued", delivery: "steer", own: true })
      expect(peer.queued()).toHaveLength(1)

      server.emit("session.inbox.delivered", { sessionID: "ses_child", inboxID: id })
      state.setPending("ses_child", [])
      expect(peer.messages()[0]).toMatchObject({ id, status: "sent" })
      expect(peer.queued()).toEqual([])
    } finally {
      peer.dispose()
    }
  })

  test("drops live unread row if cancelled or delivered before store sync", () => {
    state = fakeStore()
    server = fakeServer()
    const peer = feed()
    try {
      server.emit("subagent.peer.message", {
        sessionID: "ses_child",
        from: "user",
        text: "cancel me",
        created: 5,
        messageID: "msg_cancel_1",
      })
      expect(peer.queued().map((row) => row.id)).toEqual(["msg_cancel_1"])

      server.emit("session.inbox.cancelled", { sessionID: "ses_child", inboxID: "msg_cancel_1" })
      expect(peer.messages()).toEqual([])
      expect(peer.queued()).toEqual([])
    } finally {
      peer.dispose()
    }
  })

  test("projects chronological Q&A thread with assistant replies, preserves delivery on add, and skips self-unread", () => {
    state = fakeStore()
    server = fakeServer()
    const assistantMsg = {
      id: "msg_a1",
      type: "assistant",
      agent: "explore",
      model: { id: "m", providerID: "p" },
      content: [{ type: "text", text: "Found 2 matches" }],
      time: { created: 2 },
    }
    state.setMessages("ses_child", [
      { id: "msg_u1", type: "user", text: "Check repo", time: { created: 1 } },
      assistantMsg,
      delivered("msg_p1", "Focus on auth", 4),
      {
        id: "msg_a2",
        type: "assistant",
        agent: "explore",
        model: { id: "m", providerID: "p" },
        content: [{ type: "text", text: "Auth uses JWT" }],
        time: { created: 5, completed: 6 },
      },
    ])
    const peer = feed()
    try {
      expect(peer.thread?.()).toEqual([
        expect.objectContaining({ id: "msg_u1", role: "question", origin: "user", text: "Check repo" }),
        expect.objectContaining({ id: "msg_a1", role: "answer", text: "Found 2 matches", streaming: true }),
        expect.objectContaining({ id: "msg_p1", role: "question", origin: "peer", text: "Focus on auth", status: "sent" }),
        expect.objectContaining({ id: "msg_a2", role: "answer", text: "Auth uses JWT", streaming: false }),
      ])

      // In-place mutation of assistant content invalidates the cache without changing array identity/length.
      assistantMsg.content = [{ type: "text", text: "Found 2 matches (done)" }]
      assistantMsg.time = { created: 2, completed: 3 } as { created: number; completed?: number }
      expect(peer.thread?.()[1]).toMatchObject({
        id: "msg_a1",
        text: "Found 2 matches (done)",
        streaming: false,
      })

      // Own sends preserve `delivery: "steer"` and never bump `unread`.
      peer.add({ id: "msg_own_steer", from: "you", own: true, text: "Hurry up", created: 10 }, "steer")
      expect(peer.unread()).toBe(0)
      expect(peer.queued()[0]).toMatchObject({ id: "msg_own_steer", delivery: "steer", status: "queued" })
      expect(peer.thread?.().at(-1)).toMatchObject({
        id: "msg_own_steer",
        role: "question",
        origin: "peer",
        delivery: "steer",
        status: "queued",
      })
    } finally {
      peer.dispose()
    }
  })
})
