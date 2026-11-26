import { afterEach, describe, expect, mock, test } from "bun:test"
// Type-only: erased at compile time, so it never loads the real module (which this file mocks below).
import type { ServerConnection } from "@/runtime/server/registry"

// `bun test --conditions=solid` resolves `solid-js` to the SSR build, where render() throws "Client-only API called on
// the server side". Swap in the client build for this file's registry (we run under `--isolate`). Deep specifiers live
// in variables so `tsc` does not need declarations for them.
const clientSolidSpecifier = "solid-js/dist/solid.js"
const clientSolid = await import(clientSolidSpecifier)
mock.module("solid-js", () => clientSolid)

// `--conditions=solid` also resolves `solid-js/store` to its SSR build (`dist/server.js`), whose `setStore` only mutates
// and never notifies: a reactive pane over the real `createServerHealth` store would never re-render. The app ships the
// client store build, so swap it in too; it must be imported AFTER the solid-js mock so both share one runtime.
const clientStoreSpecifier = "solid-js/store/dist/store.js"
const clientStore = await import(clientStoreSpecifier)
mock.module("solid-js/store", () => clientStore)

const webSpecifier = "solid-js/web/dist/web.js"
const { render } = await import(webSpecifier)

const { createComponent, createRoot, createSignal, onMount } = await import("solid-js")

type HealthRow = { healthy: boolean; checking?: boolean; incompatible?: boolean; unauthorized?: boolean }

// A reactive health registry: the provider reads `global.servers.health[key]`, so a probe update must be observable as
// a signal read. The proxy tracks a version signal on every property access.
let healthRows: Record<string, HealthRow | undefined> = {}
const [healthVersion, setHealthVersion] = createSignal(0)
const health = new Proxy({} as Record<string, HealthRow | undefined>, {
  get: (_target, prop) => {
    healthVersion()

    return typeof prop === "string" ? healthRows[prop] : undefined
  },
  has: (_target, prop) => {
    healthVersion()

    return typeof prop === "string" && prop in healthRows
  },
  ownKeys: () => {
    healthVersion()

    return Object.keys(healthRows)
  },
  getOwnPropertyDescriptor: () => ({ enumerable: true, configurable: true }),
})

function setHealth(next: Record<string, HealthRow | undefined>) {
  healthRows = next
  setHealthVersion((value) => value + 1)
}

const fake = {
  retries: 0,
  ctx: { marker: "server-ctx" },
}

// The three modules the provider reads. `@opencode/ui/context` is replaced with a minimal provider so this file tests
// the provider's own control flow (health -> pane vs children), not an unrelated JSX runtime.
mock.module("@/runtime/server/registry", () => ({
  ServerConnection: {
    key: (conn: { http?: { url: string } }) => conn.http?.url ?? "",
    local: () => true,
    builtin: () => true,
  },
}))

mock.module("@/runtime/server/runtime", () => ({
  useGlobal: () => ({
    servers: {
      health,
      retry: () => {
        fake.retries += 1

        return Promise.resolve()
      },
    },
    serverCtx: () => fake.ctx,
    ensureServerCtx: () => fake.ctx,
  }),
}))

let providedCtx: unknown
mock.module("@opencode/ui/context", () => ({
  createSimpleContext: (options: { init: (props: { conn: unknown; ctx: unknown }) => unknown }) => {
    let value: unknown
    const provider = (props: { conn: unknown; ctx: unknown; children: unknown }) => {
      value = options.init({ conn: props.conn, ctx: props.ctx })
      providedCtx = value

      return props.children
    }

    return { use: () => value, provider }
  },
}))

const { pendingState, readyKey, ServerPendingPane, ServerProvider, ServerStatusPane, serverStatus } = await import(
  "@/runtime/server/current"
)

const conn = { type: "http", http: { url: "http://localhost:4096" } } as never
const hosts: HTMLElement[] = []

function mount(node: () => unknown) {
  const host = document.createElement("div")
  document.body.appendChild(host)
  hosts.push(host)

  return { host, dispose: render(node as never, host) }
}

afterEach(() => {
  for (const host of hosts.splice(0)) host.remove()
  setHealth({})
  fake.retries = 0
  providedCtx = undefined
})

describe("serverStatus", () => {
  test("undefined health is the connecting window, never healthy", () => {
    expect(serverStatus(undefined)).toBe("connecting")
  })

  test("a probe in flight is still connecting", () => {
    expect(serverStatus({ healthy: false, checking: true })).toBe("connecting")
  })

  test("a failed probe is unreachable and retryable", () => {
    expect(serverStatus({ healthy: false })).toBe("unreachable")
    expect(pendingState({ healthy: false })).toBe("unreachable")
  })

  test("incompatible is kept distinct from unreachable", () => {
    expect(serverStatus({ healthy: false, incompatible: true })).toBe("incompatible")
  })

  test("a rejected credential never becomes a retryable outage", () => {
    expect(serverStatus({ healthy: false, unauthorized: true })).toBe("connecting")
  })

  test("only a healthy probe is ok, and the pane is never ok", () => {
    expect(serverStatus({ healthy: true })).toBe("ok")
    expect(pendingState({ healthy: true })).toBe("connecting")
  })
})

describe("readyKey", () => {
  const key = "http://localhost:4096"

  test("an unprobed server is not ready", () => {
    expect(readyKey(undefined, key, undefined)).toBeUndefined()
    expect(readyKey({ healthy: false, checking: true }, key, undefined)).toBeUndefined()
  })

  test("a failed first probe is not ready", () => {
    expect(readyKey({ healthy: false }, key, undefined)).toBeUndefined()
  })

  test("a healthy probe latches its own key", () => {
    expect(readyKey({ healthy: true }, key, undefined)).toBe(key)
  })

  test("a later failed re-probe keeps the mounted shell instead of tearing it down", () => {
    expect(readyKey({ healthy: false }, key, key)).toBe(key)
  })

  test("another server never inherits the latch", () => {
    expect(readyKey({ healthy: false }, "http://other:4096", key)).toBeUndefined()
    expect(readyKey(undefined, undefined, key)).toBeUndefined()
  })
})

describe("ServerStatusPane", () => {
  test("pending health renders visible content in a live status region", () => {
    const { host } = mount(() => ServerStatusPane({ state: "connecting" }))

    const region = host.querySelector('[data-testid="server-connecting"]')
    expect(region).not.toBeNull()
    expect(region?.getAttribute("role")).toBe("status")
    expect(host.textContent ?? "").toContain("Loading")
    expect(host.querySelector('[data-testid="server-status"]')).not.toBeNull()
  })

  test("failure renders a retry control that invokes the supplied callback", () => {
    let calls = 0
    const { host } = mount(() => ServerStatusPane({ state: "unreachable", onRetry: () => (calls += 1) }))

    expect(host.querySelector('[data-testid="server-unreachable"]')).not.toBeNull()
    const button = host.querySelector<HTMLButtonElement>('button[data-testid="server-retry"]')
    expect(button).not.toBeNull()
    expect(button?.textContent ?? "").toContain("Try again")

    button?.click()
    button?.click()
    expect(calls).toBe(2)
  })

  test("incompatible keeps its own message and offers no retry", () => {
    const { host } = mount(() => ServerStatusPane({ state: "incompatible" }))

    expect(host.textContent ?? "").toContain("version isn't supported")
    expect(host.querySelector('[data-testid="server-connecting"]')).toBeNull()
    expect(host.querySelector('[data-testid="server-retry"]')).toBeNull()
  })
})

describe("ServerProvider", () => {
  test("mounts children immediately while health is undefined without blocking behind a connecting pane", () => {
    setHealth({})
    const { host } = mount(() => createComponent(ServerProvider, { conn, children: (() => "session-content") as never }))

    expect(host.querySelector('[data-testid="server-connecting"]')).toBeNull()
    expect(host.textContent ?? "").toContain("session-content")
  })

  test("unauthorized credentials still hold children behind ServerStatusPane", () => {
    setHealth({ "http://localhost:4096": { healthy: false, unauthorized: true } })
    const { host } = mount(() => createComponent(ServerProvider, { conn, children: (() => "session-content") as never }))

    expect(host.querySelector('[data-testid="server-connecting"]')).not.toBeNull()
    expect(host.textContent ?? "").not.toContain("session-content")
  })

  test("healthy false keeps children mounted and renders non-blocking retry banner calling global.servers.retry", () => {
    setHealth({ "http://localhost:4096": { healthy: false } })
    const { host } = mount(() => createComponent(ServerProvider, { conn, children: (() => "session-content") as never }))

    expect(host.textContent ?? "").toContain("session-content")
    expect(host.querySelector('[data-testid="server-unreachable-banner"]')).not.toBeNull()
    host.querySelector<HTMLButtonElement>('button[data-testid="server-retry"]')?.click()
    expect(fake.retries).toBe(1)
  })

  test("a successful health check renders children once and no status pane", () => {
    setHealth({ "http://localhost:4096": { healthy: true } })
    let mounts = 0
    const { host } = mount(() =>
      createComponent(ServerProvider, {
        conn,
        children: (() => {
          onMount(() => (mounts += 1))

          return "session-content"
        }) as never,
      }),
    )

    expect((providedCtx as { ctx?: unknown } | undefined)?.ctx).toBe(fake.ctx)
    expect(host.textContent ?? "").toContain("session-content")
    expect(host.querySelector('[data-testid="server-status"]')).toBeNull()
    expect(host.querySelector('[data-testid="server-unreachable-banner"]')).toBeNull()
    expect(mounts).toBe(1)
  })

  test("pending -> healthy keeps children mounted once without a connecting swap", async () => {
    setHealth({})
    let mounts = 0
    const { host } = mount(() =>
      createComponent(ServerProvider, {
        conn,
        children: (() => {
          onMount(() => (mounts += 1))

          return "session-content"
        }) as never,
      }),
    )

    expect(host.querySelector('[data-testid="server-connecting"]')).toBeNull()
    expect(host.textContent ?? "").toContain("session-content")
    setHealth({ "http://localhost:4096": { healthy: true } })
    await Promise.resolve()

    expect(host.textContent ?? "").toContain("session-content")
    expect(host.querySelector('[data-testid="server-status"]')).toBeNull()
    expect(mounts).toBe(1)
  })

  test("a later failed poll keeps the mounted children and shows the non-blocking retry banner", async () => {
    setHealth({ "http://localhost:4096": { healthy: true } })
    let mounts = 0
    const { host } = mount(() =>
      createComponent(ServerProvider, {
        conn,
        children: (() => {
          onMount(() => (mounts += 1))

          return "session-content"
        }) as never,
      }),
    )

    setHealth({ "http://localhost:4096": { healthy: false } })
    await Promise.resolve()

    expect(host.textContent ?? "").toContain("session-content")
    expect(host.querySelector('[data-testid="server-status"]')).toBeNull()
    expect(host.querySelector('[data-testid="server-unreachable-banner"]')).not.toBeNull()
    expect(mounts).toBe(1)
  })

  test("an incompatible server still opens the pane so its own upgrade panel can render", () => {
    setHealth({ "http://localhost:4096": { healthy: false, incompatible: true } })
    const { host } = mount(() => createComponent(ServerProvider, { conn, children: (() => "session-content") as never }))

    expect(host.textContent ?? "").toContain("session-content")
    expect(host.querySelector('[data-testid="server-status"]')).toBeNull()
  })
})

// --- F3: bounded failure on the REAL probe path ----------------------------------------------------------------
//
// The failing e2e was not a pre-seeded `healthy:false` row: the page aborts every /api/** request, so the probe's
// fetch rejects with a TypeError ("Failed to fetch"). These tests wire the real `checkServerHealth` -> real
// `createServerHealth` store -> mounted reactive pane, with no hand-written health row, and assert the bounded
// connecting -> visible error+retry transition plus a retry that re-probes.

const { checkServerHealth, createServerHealth } = await import("@/runtime/server/health")

const probeKey = "http://localhost:4096" as ServerConnection.Key
const disposers: (() => void)[] = []

function disposerRoot<T>(fn: () => T): T {
  return createRoot((dispose) => {
    disposers.push(dispose)

    return fn()
  })
}

/** Let queued Solid effects and probe microtasks run; the pane is rarely updated by the first tick. */
async function settle() {
  for (let tick = 0; tick < 8; tick += 1) await Promise.resolve()
  await new Promise((resolve) => setTimeout(resolve, 0))
  for (let tick = 0; tick < 8; tick += 1) await Promise.resolve()
}

// What Chromium rejects with when Playwright aborts the route (`route.abort("failed")`).
function abortedFetch(counter: { calls: number }) {
  return (async () => {
    counter.calls += 1
    throw new TypeError("Failed to fetch")
  }) as unknown as typeof globalThis.fetch
}

function healthFor(check: Parameters<typeof createServerHealth>[2]) {
  return disposerRoot(() =>
    createServerHealth(
      () => [{ type: "http", http: { url: probeKey } } as never],
      () => true,
      check,
    ),
  )
}

afterEach(() => {
  for (const dispose of disposers.splice(0)) dispose()
})

describe("F3 bounded failure on the real probe path", () => {
  test("an aborted probe leaves connecting for a visible error+retry, and retry re-probes", async () => {
    const counter = { calls: 0 }
    const health = healthFor((http) => checkServerHealth(http, abortedFetch(counter), { retryCount: 1, retryDelayMs: 1 }))
    const pane = disposerRoot(() =>
      ServerPendingPane({ state: () => pendingState(health.status[probeKey]), onRetry: () => void health.retry() }),
    )

    const { host } = mount(() => pane)

    // Pending stays visible (never an empty node) before the probe settles.
    expect(host.querySelector('[data-testid="server-connecting"]')).not.toBeNull()

    await health.retry()
    await settle()

    // The real classifier retried the transport failure, then committed the outage.
    expect(counter.calls).toBeGreaterThan(1)
    expect(host.querySelector('[data-testid="server-unreachable"]')).not.toBeNull()
    expect(host.querySelector('[data-testid="server-connecting"]')).toBeNull()

    const button = host.querySelector<HTMLButtonElement>('button[data-testid="server-retry"]')
    expect(button).not.toBeNull()
    const before = counter.calls
    button?.click()
    await settle()

    // Retry calls the real `retry()`, which re-runs the probe rather than re-rendering a stale pane.
    expect(counter.calls).toBeGreaterThan(before)
  })

  test("a probe that rejects is committed as a retryable outage, never left pending", async () => {
    const health = healthFor(async () => {
      throw new Error("probe blew up")
    })

    await health.retry()
    await settle()

    expect(health.status[probeKey]?.healthy).toBe(false)
    expect(serverStatus(health.status[probeKey])).toBe("unreachable")
    expect(pendingState(health.status[probeKey])).toBe("unreachable")
  })
})
