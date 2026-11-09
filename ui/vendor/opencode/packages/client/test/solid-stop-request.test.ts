import { expect, test } from "bun:test"
import { createRoot } from "solid-js"
import { createData, type CreateDataInput } from "../src/solid"
import { OpenCode, type OpenCodeEvent, type SessionInfo } from "../src/promise"

const sessionID = "ses_stop"

const info: SessionInfo = {
  id: sessionID,
  projectID: "project",
  cost: 0,
  tokens: { input: 0, output: 0, reasoning: 0, cache: { read: 0, write: 0 } },
  outcome: "succeeded",
  time: { created: 0, updated: 0 },
  location: { directory: "/project" },
}

function fixture() {
  const listeners = new Set<Parameters<CreateDataInput["event"]["listen"]>[0]>()
  const api = OpenCode.make({
    baseUrl: "http://opencode.local",
    fetch: async (input, init) => {
      const request = input instanceof Request ? input : new Request(input, init)
      if (new URL(request.url).pathname === `/api/session/${sessionID}`) return Response.json({ data: info })
      return Response.json({ data: [] })
    },
  })
  let seq = 0
  return createRoot((dispose) => {
    const data = createData({
      api: () => api,
      directory: "/project",
      event: {
        on: () => () => {},
        listen(handler) {
          listeners.add(handler)
          return () => listeners.delete(handler)
        },
      },
    })
    const emit = (type: string, payload: Record<string, unknown> = {}) => {
      seq++
      const event = {
        id: `evt_${seq}`,
        type,
        created: seq,
        durable: { aggregateID: sessionID, seq, version: 1 },
        data: { sessionID, ...payload },
      } as OpenCodeEvent
      listeners.forEach((listener) => listener({ name: event.type, details: event }))
    }
    return {
      data,
      dispose,
      started: () => emit("session.execution.started"),
      interrupted: () => emit("session.execution.interrupted", { reason: "user" }),
    }
  })
}

test("status is idle while a stop is requested", () => {
  const setup = fixture()
  try {
    setup.data.session.remember(info)
    setup.started()
    expect(setup.data.session.status(sessionID)).toBe("running")
    expect(setup.data.session.active()).toContain(sessionID)

    setup.data.session.requestStop([sessionID])

    expect(setup.data.session.status(sessionID)).toBe("idle")
    expect(setup.data.session.active()).not.toContain(sessionID)
  } finally {
    setup.dispose()
  }
})

test("a new execution start clears the stop request", () => {
  const setup = fixture()
  try {
    setup.data.session.remember(info)
    setup.started()
    setup.data.session.requestStop([sessionID])
    expect(setup.data.session.status(sessionID)).toBe("idle")
    expect(setup.data.session.isStopRequested(sessionID)).toBe(true)

    setup.started()

    expect(setup.data.session.status(sessionID)).toBe("running")
    expect(setup.data.session.isStopRequested(sessionID)).toBe(false)
  } finally {
    setup.dispose()
  }
})

test("a terminal event clears the stop request", () => {
  const setup = fixture()
  try {
    setup.data.session.remember(info)
    setup.started()
    setup.data.session.requestStop([sessionID])
    expect(setup.data.session.status(sessionID)).toBe("idle")
    expect(setup.data.session.isStopRequested(sessionID)).toBe(true)

    setup.interrupted()

    expect(setup.data.session.status(sessionID)).toBe("idle")
    expect(setup.data.session.isStopRequested(sessionID)).toBe(false)
  } finally {
    setup.dispose()
  }
})
