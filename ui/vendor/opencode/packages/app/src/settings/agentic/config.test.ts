import { beforeEach, describe, expect, mock, test } from "bun:test"
import { createRoot } from "solid-js"

type Document = { type: "document"; info: Record<string, unknown> }

let documents: Document[] = []
let queryError = false
let scope = "local"
const invalidated: unknown[] = []
const refetches: string[] = []

// The server build of Solid has no reactive host, so this test replaces only the query layer the settings
// store reads; `createAgenticConfig` itself and every merge/display path stay real.
mock.module("@tanstack/solid-query", () => ({
  useQuery: () => ({
    get data() {
      return documents
    },
    get isPending() {
      return false
    },
    get isError() {
      return queryError
    },
    refetch: async () => {
      refetches.push("refetch")
    },
  }),
  useQueryClient: () => ({
    invalidateQueries: async (input: unknown) => {
      invalidated.push(input)
    },
  }),
}))

type SDK = { scope: string; http: { url: string; password?: string } }
const sdk: SDK = { scope, http: { url: "http://server.test" } }

mock.module("@/runtime/server/client", () => ({ useServerSDK: () => sdk }))

const { createAgenticConfig, isConfigObject, mergeConfig, mergeDocuments, serverMessage } = await import("./config")

/** Replaces the global fetch for one test and records the requests it received. */
function stubFetch(respond: (request: Request) => Response | Promise<Response>) {
  const requests: Request[] = []
  const bodies: string[] = []

  globalThis.fetch = (async (input: string | URL | Request, init?: RequestInit) => {
    const request = input instanceof Request ? input : new Request(input, init)

    bodies.push(await request.clone().text())
    requests.push(request)

    return respond(request)
  }) as typeof globalThis.fetch

  return { requests, bodies }
}

function create() {
  return createRoot((dispose) => ({ config: createAgenticConfig(), dispose }))
}

beforeEach(() => {
  documents = []
  queryError = false
  invalidated.length = 0
  refetches.length = 0
  // Every test gets a fresh scope so the module's pending-section overlay starts empty.
  scope = `test:${crypto.randomUUID()}`
  sdk.scope = scope
  sdk.http = { url: "http://server.test" }
  sessionStorage.clear()
})

describe("mergeConfig", () => {
  test("deep-merges objects and replaces arrays and scalars", () => {
    const base = {
      observer: { tick_ms: 1000, model: "test/alpha", thresholds: { stalls: 2, repairs: 5 } },
      lanes: { order: ["cargo", "node"] },
      model: "test/base",
    }

    expect(
      mergeConfig(base, {
        observer: { tick_ms: 500, thresholds: { repairs: 9 } },
        lanes: { order: ["gpu"] },
        model: { providerID: "test", model: "beta" },
      }),
    ).toEqual({
      observer: { tick_ms: 500, model: "test/alpha", thresholds: { stalls: 2, repairs: 9 } },
      lanes: { order: ["gpu"] },
      model: { providerID: "test", model: "beta" },
    })
    // The inputs are not mutated.
    expect(base.observer.tick_ms).toBe(1000)
    expect(base.lanes.order).toEqual(["cargo", "node"])
  })

  test("keeps keys only present in the base and treats arrays as scalars", () => {
    expect(mergeConfig({ a: 1, list: [1, 2] }, { list: [3] })).toEqual({ a: 1, list: [3] })
    expect(mergeConfig({}, { list: [1] })).toEqual({ list: [1] })
  })
})

describe("mergeDocuments", () => {
  test("applies only later documents, in order, and skips malformed entries", () => {
    expect(
      mergeDocuments([
        { type: "document", info: { observer: { tick_ms: 1000, model: "first" } } },
        { type: "file", info: { observer: { tick_ms: 1 } } },
        { type: "document", info: { observer: { tick_ms: 500 } } },
        null,
        { type: "document" },
        { type: "document", info: "nope" },
      ]),
    ).toEqual({ observer: { tick_ms: 500, model: "first" } })
    expect(mergeDocuments([])).toEqual({})
  })
})

describe("save", () => {
  test("patches the merged section, layers it over the fetched document, and drains the body", async () => {
    documents = [{ type: "document", info: { observer: { tick_ms: 1000, model: "test/alpha" }, memory: { on: true } } }]
    let response: Response | undefined
    const fetch = stubFetch(() => {
      response = new Response("ignored", { status: 200, headers: { "x-opencode-restart": "required" } })

      return response
    })
    const { config, dispose } = create()

    try {
      expect(config.section("observer")).toEqual({ tick_ms: 1000, model: "test/alpha" })

      const saved = await config.save("observer", { tick_ms: 500 })

      expect(saved).toBe(true)
      expect(fetch.requests).toHaveLength(1)
      const request = fetch.requests[0]!
      expect(request.method).toBe("PATCH")
      expect(new URL(request.url).pathname).toBe("/api/experimental/config")
      expect(request.headers.get("content-type")).toBe("application/json")
      expect(JSON.parse(fetch.bodies[0]!)).toEqual({ observer: { tick_ms: 500, model: "test/alpha" } })
      // Only the written section is sent; the fetched document's other keys are not.
      expect(Object.keys(JSON.parse(fetch.bodies[0]!))).toEqual(["observer"])

      expect(response?.bodyUsed).toBe(true)
      expect(config.restart()).toBe(true)
      expect(config.saving()).toBe(false)

      // Display still shows the server snapshot; the section shows the pending write layered over it.
      expect(config.display()).toEqual({ observer: { tick_ms: 1000, model: "test/alpha" }, memory: { on: true } })
      expect(config.section("observer")).toEqual({ tick_ms: 500, model: "test/alpha" })
      expect(invalidated).toEqual([{ queryKey: [scope, "settings-agentic-config"] }])
    } finally {
      dispose()
    }
  })

  test("does not request a restart when the header is absent", async () => {
    documents = [{ type: "document", info: { observer: { tick_ms: 1000 } } }]
    stubFetch(() => new Response("ignored", { status: 200 }))
    const { config, dispose } = create()

    try {
      expect(await config.save("observer", { tick_ms: 750 })).toBe(true)
      expect(config.restart()).toBe(false)
    } finally {
      dispose()
    }
  })

  test("sends the basic authorization password when the server has one", async () => {
    sdk.http = { url: "http://server.test/", password: "secret" }
    documents = []
    const fetch = stubFetch(() => new Response("ignored", { status: 200 }))
    const { config, dispose } = create()

    try {
      expect(await config.save("lanes", { cargo: { concurrency: 2 } })).toBe(true)
      expect(fetch.requests[0]!.headers.get("authorization")).toBe(`Basic ${btoa("opencode:secret")}`)
      expect(fetch.requests[0]!.url).toBe("http://server.test/api/experimental/config")
    } finally {
      dispose()
    }
  })

  test("surfaces the server error envelope and keeps the pending write out of the display", async () => {
    documents = [{ type: "document", info: { observer: { tick_ms: 1000 } } }]
    stubFetch(() =>
      Response.json({ _tag: "ConfigError", message: "observer.tick_ms must be positive" }, { status: 400 }),
    )
    const { config, dispose } = create()

    try {
      expect(await config.save("observer", { tick_ms: -1 })).toBe(false)
      expect(config.error()).toBe("observer.tick_ms must be positive")
      expect(config.restart()).toBe(false)
      expect(config.section("observer")).toEqual({ tick_ms: 1000 })
      expect(invalidated).toEqual([])

      config.clearError()
      expect(config.error()).toBeUndefined()
    } finally {
      dispose()
    }
  })

  test("reports a network failure without claiming success", async () => {
    documents = []
    globalThis.fetch = (async () => {
      throw new TypeError("Failed to fetch")
    }) as typeof globalThis.fetch
    const { config, dispose } = create()

    try {
      expect(await config.save("observer", { tick_ms: 500 })).toBe(false)
      expect(config.error()).toBe("Failed to fetch")
      expect(config.saving()).toBe(false)
    } finally {
      dispose()
    }
  })
})

describe("reload", () => {
  test("asks the query layer for a fresh document", () => {
    documents = []
    const { config, dispose } = create()

    try {
      config.reload()
      expect(refetches).toEqual(["refetch"])
    } finally {
      dispose()
    }
  })
})

describe("isConfigObject", () => {
  test("accepts plain objects only", () => {
    expect(isConfigObject({})).toBe(true)
    expect(isConfigObject({ nested: 1 })).toBe(true)
    expect(isConfigObject([])).toBe(false)
    expect(isConfigObject(null)).toBe(false)
    expect(isConfigObject(undefined)).toBe(false)
    expect(isConfigObject("value")).toBe(false)
    expect(isConfigObject(3)).toBe(false)
  })
})

describe("serverMessage", () => {
  test("prefers the top-level error message", async () => {
    await expect(serverMessage(Response.json({ _tag: "RequestError", message: "bad input" }, { status: 400 }))).resolves.toBe(
      "bad input",
    )
    await expect(
      serverMessage(Response.json({ _tag: "Conflict", message: "session is busy" }, { status: 409 })),
    ).resolves.toBe("session is busy")
    await expect(
      serverMessage(Response.json({ _tag: "Unavailable", message: "observer disabled" }, { status: 503 })),
    ).resolves.toBe("observer disabled")
  })

  test("reads a nested data.message envelope", async () => {
    await expect(
      serverMessage(Response.json({ _tag: "Error", data: { message: "nested reason" } }, { status: 500 })),
    ).resolves.toBe("nested reason")
  })

  test("falls back to the status line when the body has no message", async () => {
    await expect(serverMessage(Response.json({ _tag: "RequestError" }, { status: 400, statusText: "Bad Request" }))).resolves
      .toBe("400 Bad Request")
    await expect(serverMessage(Response.json({ message: "" }, { status: 503, statusText: "Service Unavailable" })))
      .resolves.toBe("503 Service Unavailable")
    await expect(serverMessage(Response.json({ data: { message: 3 } }, { status: 409, statusText: "Conflict" }))).resolves
      .toBe("409 Conflict")
  })

  test("falls back when the body is not JSON or not an object", async () => {
    await expect(serverMessage(new Response("plain text", { status: 502, statusText: "Bad Gateway" }))).resolves.toBe(
      "502 Bad Gateway",
    )
    await expect(serverMessage(Response.json(["message"], { status: 500, statusText: "Internal Server Error" })))
      .resolves.toBe("500 Internal Server Error")
    await expect(serverMessage(new Response(null, { status: 204 }))).resolves.toBe("204")
  })

  test("surfaces the server's message from a 200 error body", async () => {
    await expect(serverMessage(Response.json({ _tag: "Error", message: "observer is disabled" }, { status: 200 })))
      .resolves.toBe("observer is disabled")
  })
})
