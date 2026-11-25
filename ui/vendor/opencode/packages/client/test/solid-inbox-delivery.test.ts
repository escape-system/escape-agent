// Behavioral regressions for inbox promotion and history preservation in the client data layer.
// Runs with `bun test` from packages/client only (no app solid/happydom preloads): these paths are
// store logic, not rendering.
import { expect, test } from "bun:test"
import { createRoot } from "solid-js"
import { createData, type CreateDataInput } from "../src/solid"
import { OpenCode, type OpenCodeEvent, type SessionInboxItem } from "../src/promise"

const SESSION = "ses_delivery"

async function wait(predicate: () => boolean, timeout = 1000) {
  const start = Date.now()
  while (!predicate()) {
    if (Date.now() - start > timeout) throw new Error("wait timed out")
    await Bun.sleep(5)
  }
}

function fixture(fetch: (request: Request) => Promise<Response>) {
  const listeners = new Set<Parameters<CreateDataInput["event"]["listen"]>[0]>()
  const requests: URL[] = []
  const api = OpenCode.make({
    baseUrl: "http://opencode.local",
    fetch: (input, init) => {
      const request = input instanceof Request ? input : new Request(input, init)
      requests.push(new URL(request.url))
      return fetch(request)
    },
  })
  const setup = createRoot((dispose) => ({
    data: createData({
      api: () => api,
      directory: "/project",
      event: {
        on: () => () => {},
        listen(handler) {
          listeners.add(handler)
          return () => listeners.delete(handler)
        },
      },
    }),
    requests,
    emit: (event: OpenCodeEvent) => listeners.forEach((listener) => listener({ name: event.type, details: event })),
    dispose,
  }))
  return setup
}

const queuedItem: SessionInboxItem = {
  id: "msg_delivered",
  sessionID: SESSION,
  type: "user",
  delivery: "steer",
  time: { created: 1 },
  payload: { text: "Delivered prompt" },
}

const enqueued = (item: SessionInboxItem): OpenCodeEvent =>
  ({
    id: `evt_enqueued_${item.id}`,
    type: "session.inbox.enqueued",
    created: item.time.created,
    durable: { aggregateID: SESSION, seq: 1, version: 1 },
    data: {
      sessionID: SESSION,
      inboxID: item.id,
      item: { type: item.type, delivery: item.delivery, payload: item.payload },
    },
  }) as unknown as OpenCodeEvent

const delivered = (inboxID: string, created: number): OpenCodeEvent =>
  ({
    id: `evt_delivered_${inboxID}`,
    type: "session.inbox.delivered",
    created,
    durable: { aggregateID: SESSION, seq: 2, version: 1 },
    data: { sessionID: SESSION, inboxID },
  }) as unknown as OpenCodeEvent

// R1 regression: promotion used to splice the row out and push it to the tail, so a steer the
// reader had already seen jumped below newer output. It must keep its chronological slot.
test("a delivered steer keeps its chronological position in the transcript", async () => {
  const setup = fixture(async () => Response.json({ data: [], cursor: {} }))
  try {
    setup.emit(enqueued(queuedItem))
    expect(setup.data.session.message.list(SESSION).map((item) => item.id)).toEqual(["msg_delivered"])
    // Newer output arrives while the prompt is still pending.
    setup.emit({
      id: "evt_synthetic",
      type: "session.synthetic",
      created: 2,
      durable: { aggregateID: SESSION, seq: 2, version: 1 },
      data: { sessionID: SESSION, text: "Tool output" },
    } as unknown as OpenCodeEvent)
    expect(setup.data.session.message.list(SESSION).map((item) => item.id)).toEqual(["msg_delivered", "msg_synthetic"])

    setup.emit(delivered("msg_delivered", 3))

    const rows = setup.data.session.message.list(SESSION)
    expect(rows.map((item) => item.id)).toEqual(["msg_delivered", "msg_synthetic"])
    expect(rows[0]?.time.created).toBe(3)
    expect(setup.data.session.input.has(SESSION, "msg_delivered")).toBe(false)
  } finally {
    setup.dispose()
  }
})

// R1b: a promotion whose transcript row was never rendered (evicted while queued) is rebuilt from
// the pending payload instead of being dropped.
test("a delivery without a rendered row materializes from the pending item exactly once", async () => {
  const pending = Promise.withResolvers<Response>()
  const setup = fixture(() => pending.promise)
  try {
    // Hydrate pending through the API only: the read also materializes the transcript row.
    const reading = setup.data.session.pending.sync(SESSION)
    pending.resolve(Response.json({ data: [queuedItem] }))
    await reading
    expect(setup.data.session.pending.list(SESSION).map((item) => item.id)).toEqual(["msg_delivered"])
    expect(setup.data.session.message.list(SESSION).map((item) => item.id)).toEqual(["msg_delivered"])

    setup.emit(delivered("msg_delivered", 9))
    const rows = setup.data.session.message.list(SESSION)
    expect(rows.map((item) => item.id)).toEqual(["msg_delivered"])
    expect(rows).toHaveLength(1)
    expect(rows[0]?.time.created).toBe(9)
    expect(setup.data.session.pending.list(SESSION)).toEqual([])
  } finally {
    pending.resolve(Response.json({ data: [] }))
    setup.dispose()
  }
})

// R2 regression: a delivery.changed event that arrives before the pending snapshot left no row to
// update and was never read back, so the flag was lost until a manual refresh. Now it triggers one
// coalesced pending resync carrying server truth.
test("an unseen delivery.changed triggers one coalesced pending resync", async () => {
  const snapshot = Promise.withResolvers<Response>()
  const setup = fixture(() => snapshot.promise)
  try {
    setup.emit({
      id: "evt_delivery_changed",
      type: "session.inbox.delivery.changed",
      created: 2,
      durable: { aggregateID: SESSION, seq: 1, version: 1 },
      data: { sessionID: SESSION, inboxID: "msg_delivered", delivery: "queue" },
    } as unknown as OpenCodeEvent)

    await wait(() => setup.requests.length === 1)
    expect(setup.requests[0]?.pathname).toBe(`/api/session/${SESSION}/inbox`)
    snapshot.resolve(Response.json({ data: [{ ...queuedItem, delivery: "queue" }] }))

    await wait(() => setup.data.session.pending.list(SESSION).length === 1)
    expect(setup.data.session.pending.list(SESSION)).toEqual([{ ...queuedItem, delivery: "queue" }])
    expect(setup.data.session.message.list(SESSION).map((item) => item.id)).toEqual(["msg_delivered"])
    // One read for the burst, not one per event.
    expect(setup.requests.filter((url) => url.pathname.endsWith("/inbox"))).toHaveLength(1)
  } finally {
    snapshot.resolve(Response.json({ data: [] }))
    setup.dispose()
  }
})

// R3 regression: message.sync replaced the store with the first page, dropping pages loadMore had
// already fetched and resetting the cursor, which rekeys the virtualizer and throws the reader to
// the top. Older pages and their cursor must survive a later first-page revalidation.
test("message.sync preserves older pages and the history cursor", async () => {
  const setup = fixture(async (request) => {
    const cursor = new URL(request.url).searchParams.get("cursor")
    if (cursor === "older")
      return Response.json({
        data: [{ id: "msg_2", type: "user", text: "second", time: { created: 2 } }],
        cursor: { next: "oldest" },
      })
    if (cursor === "oldest")
      return Response.json({
        data: [{ id: "msg_1", type: "user", text: "first", time: { created: 1 } }],
        cursor: {},
      })
    return Response.json({
      data: [{ id: "msg_3", type: "user", text: "third", time: { created: 3 } }],
      cursor: { next: "older" },
    })
  })
  try {
    await setup.data.session.message.sync(SESSION)
    await setup.data.session.message.loadMore(SESSION)
    expect(setup.data.session.message.list(SESSION).map((item) => item.id)).toEqual(["msg_2", "msg_3"])

    // A live revalidation of the first page must not discard the older page.
    setup.data.session.message.invalidate(SESSION)
    await setup.data.session.message.sync(SESSION)

    expect(setup.data.session.message.list(SESSION).map((item) => item.id)).toEqual(["msg_2", "msg_3"])
    expect(setup.data.session.message.more(SESSION)).toBe(true)

    // The kept cursor still walks the remaining history instead of re-reading the first page.
    await setup.data.session.message.loadMore(SESSION)
    expect(setup.requests.at(-1)?.searchParams.get("cursor")).toBe("oldest")
    expect(setup.data.session.message.list(SESSION).map((item) => item.id)).toEqual(["msg_1", "msg_2", "msg_3"])
    expect(setup.data.session.message.more(SESSION)).toBe(false)
  } finally {
    setup.dispose()
  }
})

// R4 regression: `session.created` completes `session.pending:<id>` so the initial racy read is
// skipped (the documented band-aid). That completion also made every LATER read a no-op, so a
// session created in this tab never hydrated its durable inbox: the queue strip stayed empty
// although `GET /api/session/:id/inbox` returned the queued row. `pending.hydrate` must force
// exactly one durable read and publish it to `pending.list`.
test("a completed pending key still hydrates on demand", async () => {
  const requests: string[] = []
  const listeners = new Set<(event: any) => void>()
  const queued: SessionInboxItem[] = [
    {
      id: "msg_hydrated",
      sessionID: SESSION,
      type: "user",
      delivery: "queue",
      time: { created: 5 },
      payload: { text: "queued behind work" },
    },
    {
      id: "msg_hydrated_peer",
      sessionID: SESSION,
      type: "synthetic",
      delivery: "queue",
      time: { created: 6 },
      payload: { text: "peer note while busy" },
    },
  ]
  const api = OpenCode.make({
    baseUrl: "http://opencode.local",
    fetch: async (input: any) => {
      const url = new URL(String(input instanceof Request ? input.url : input))
      requests.push(url.pathname)
      if (url.pathname.endsWith("/inbox")) return Response.json({ data: queued })
      return Response.json({ data: [] })
    },
  })
  const setup = createRoot((dispose) => {
    const data = createData({
      api: () => api,
      directory: "/project",
      event: {
        on: () => () => {},
        listen(handler: any) {
          listeners.add(handler)
          return () => listeners.delete(handler)
        },
      },
    })
    return { data, dispose }
  })
  try {
    // The band-aid: the session.created replay completes the pending key.
    listeners.forEach((listener) =>
      listener({
        name: "session.created",
        details: {
          id: "evt_created",
          type: "session.created",
          created: 1,
          durable: { aggregateID: SESSION, seq: 1, version: 1 },
          data: {
            sessionID: SESSION,
            id: SESSION,
            title: "hydrate",
            time: { created: 1, updated: 1 },
            location: { directory: "/project", workspaceID: "ws" },
          },
        },
      }),
    )
    // Session records are not part of this test; the completion of the pending key is what matters.
    await Bun.sleep(20)

    // A plain sync honours the completed key and reads nothing.
    await setup.data.session.pending.sync(SESSION)
    expect(requests.filter((path) => path.endsWith("/inbox"))).toHaveLength(0)
    expect(setup.data.session.pending.list(SESSION)).toEqual([])

    // Hydration is the explicit durable read: one request, and both the queued user row and the
    // queued peer/synthetic row become visible.
    await setup.data.session.pending.hydrate(SESSION)
    expect(requests.filter((path) => path.endsWith("/inbox"))).toHaveLength(1)
    expect(setup.data.session.pending.list(SESSION).map((item) => item.id)).toEqual([
      "msg_hydrated",
      "msg_hydrated_peer",
    ])
    expect(
      setup.data.session.pending
        .list(SESSION)
        .filter((item) => item.delivery === "queue" && (item.type === "user" || item.type === "synthetic"))
        .map((item) => item.id),
    ).toEqual(["msg_hydrated", "msg_hydrated_peer"])

    // It stays bounded: a second hydrate is a normal (complete) read, not a duplicate subscription.
    await setup.data.session.pending.hydrate(SESSION)
    expect(requests.filter((path) => path.endsWith("/inbox"))).toHaveLength(2)
    expect(setup.data.session.pending.list(SESSION).map((item) => item.id)).toEqual([
      "msg_hydrated",
      "msg_hydrated_peer",
    ])
  } finally {
    setup.dispose()
  }
})
