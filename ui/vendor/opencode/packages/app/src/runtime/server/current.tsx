import { createComponent, createEffect, createMemo, type JSX, type ParentProps } from "solid-js"
import { createSimpleContext } from "@opencode/ui/context"
import { ServerConnection } from "./registry"
import { useGlobal, type ServerCtx } from "./runtime"

const context = createSimpleContext({
  name: "Server",
  init: (props: { conn: ServerConnection.Any; ctx: ServerCtx }) => {
    const conn = props.conn
    const key = ServerConnection.key(conn)

    const global = useGlobal()

    return {
      conn,
      key,
      isLocal: ServerConnection.local(props.conn),
      ctx: props.ctx,
      get health() {
        return global.servers.health[key]
      },
    }
  },
})

export const useServer = context.use

type Health = { healthy: boolean; checking?: boolean; incompatible?: boolean; unauthorized?: boolean } | undefined

/** How a health record maps to visible UI. `undefined` is the whole first-probe window; it is never "healthy". */
export type ServerStatus = "ok" | "connecting" | "unreachable" | "incompatible"

export function serverStatus(health: Health): ServerStatus {
  if (health?.healthy) return "ok"
  if (health?.incompatible) return "incompatible"
  // A rejected credential and a probe still in flight are waits, not retryable outages: their flows own the screen.
  if (!health || health.checking || health.unauthorized) return "connecting"

  return "unreachable"
}

/** The pane a server shows while it cannot render its children yet. Never `ok`. */
export function pendingState(health: Health): Exclude<ServerStatus, "ok"> {
  const status = serverStatus(health)

  return status === "ok" ? "connecting" : status
}

/**
 * The connection key the shell may treat as ready. Once a key reported healthy it stays ready: a later failed re-probe
 * must show its error inside the mounted route, not tear the live session (and its scroll position) out of the shell.
 * A different key never inherits the latch, so switching to a server that has not answered yet shows the skeleton.
 */
export function readyKey(health: Health, key: string | undefined, previous: string | undefined): string | undefined {
  if (!key) return undefined
  if (health?.healthy) return key

  return previous === key ? previous : undefined
}

const copy = {
  connecting: { title: "Loading…", detail: "Waiting for the server to answer before loading this workspace." },
  unreachable: {
    title: "Can't reach this server",
    detail: "It isn't responding. Check that it is running, then try again.",
  },
  incompatible: {
    title: "This server version isn't supported",
    detail: "Upgrade the server to the current Escape Agent build to continue.",
  },
} as const

/**
 * The visible fallback. Built as plain DOM so it exists synchronously and can never be an empty node; retry is offered
 * only for an outage (a version mismatch needs an upgrade, a rejected credential has its own sign-in flow).
 */
export function ServerStatusPane(props: {
  state: Exclude<ServerStatus, "ok">
  title?: string
  detail?: string
  action?: string
  onRetry?: () => void
}): HTMLElement {
  const text = copy[props.state]
  const root = document.createElement("div")
  root.setAttribute("data-testid", "server-status")
  root.setAttribute("data-state", props.state)
  root.className = "flex min-h-0 flex-1 flex-col items-center justify-center gap-3 p-6 text-center"

  const region = document.createElement("div")
  region.setAttribute("role", "status")
  region.setAttribute("aria-live", "polite")
  region.setAttribute("data-testid", props.state === "connecting" ? "server-connecting" : `server-${props.state}`)
  region.className = "flex flex-col items-center gap-2"

  const title = document.createElement("div")
  title.className = "text-16-medium text-text max-w-md"
  title.textContent = props.title ?? text.title

  const detail = document.createElement("div")
  detail.className = "text-13-regular text-text-weak max-w-md"
  detail.textContent = props.detail ?? text.detail

  region.append(title, detail)
  root.append(region)

  if (props.state === "connecting") {
    const track = document.createElement("div")
    track.setAttribute("aria-hidden", "true")
    track.className = "h-1 w-40 overflow-hidden rounded-full bg-white/10"
    const bar = document.createElement("div")
    bar.className = "h-full w-1/3 animate-pulse rounded-full bg-white/40"
    track.append(bar)
    root.append(track)
  }

  if (props.state === "unreachable") {
    const button = document.createElement("button")
    button.type = "button"
    button.setAttribute("data-testid", "server-retry")
    button.className =
      "rounded-md border border-white/15 bg-white/5 px-3 py-1.5 text-13-medium text-text hover:bg-white/10"
    button.textContent = props.action ?? "Try again"
    button.addEventListener("click", () => props.onRetry?.())
    root.append(button)
  }

  return root
}

/**
 * A pane that follows its health state. `ServerStatusPane` builds plain DOM once, so calling it inline in another
 * component (as the shell fallback did) froze the state it was created with: the pane kept saying "connecting" after
 * the probe settled and never grew a retry control. This component owns the swap explicitly, so it works under every
 * JSX runtime: the wrapper (`display: contents`, so the parent's flex layout is unchanged) always holds the pane for
 * the current state, and only the state string can replace it. A settled failed probe therefore becomes a visible
 * error+retry while a pending probe stays visible.
 */
export function ServerPendingPane(props: {
  state: () => Exclude<ServerStatus, "ok">
  onRetry?: () => void
}): HTMLElement {
  const wrapper = document.createElement("div")
  wrapper.className = "contents"

  let mounted: HTMLElement | undefined
  let mountedState: Exclude<ServerStatus, "ok"> | undefined

  createEffect(() => {
    const state = props.state()

    // Reuse the mounted pane while the state is unchanged, so its identity (and any focus) survives unrelated writes.
    if (mounted && mountedState === state) return
    mounted = ServerStatusPane({ state, onRetry: props.onRetry })
    mountedState = state
    wrapper.replaceChildren(mounted)
  })

  return wrapper
}

function createUnreachableBanner(unreachable: () => boolean, onRetry: () => void): HTMLElement {
  const host = document.createElement("div")
  host.className = "contents"
  let mounted: HTMLElement | undefined

  createEffect(() => {
    if (!unreachable()) {
      if (mounted) {
        mounted = undefined
        host.replaceChildren()
      }

      return
    }

    if (mounted) return
    const banner = document.createElement("div")
    banner.setAttribute("role", "alert")
    banner.setAttribute("data-testid", "server-unreachable-banner")
    banner.className =
      "fixed bottom-3 right-3 z-50 flex items-center gap-2 rounded-lg border border-white/15 bg-v2-background-bg-base/95 px-3 py-2 text-[12px] text-v2-text-text-base shadow-lg"

    const text = document.createElement("span")
    text.textContent = copy.unreachable.title

    const button = document.createElement("button")
    button.type = "button"
    button.setAttribute("data-testid", "server-retry")
    button.className = "rounded border border-white/15 bg-white/5 px-2 py-0.5 text-[12px] hover:bg-white/10"
    button.textContent = "Try again"
    button.addEventListener("click", onRetry)

    banner.append(text, button)
    mounted = banner
    host.replaceChildren(banner)
  })

  return host
}

// Must be keyed by connection. A server that rejected our credentials gets a new controller, so the children remount
// with it. They wait until the server is healthy: a controller created before its new credentials fails every load.
export function ServerProvider(props: ParentProps<{ conn: ServerConnection.Any }>): JSX.Element {
  const global = useGlobal()
  const key = ServerConnection.key(props.conn)

  const ctx = createMemo<ServerCtx | undefined>((previous) => {
    const health = global.servers.health[key]

    // Mount children immediately without blocking behind a connecting splash; rejected credentials still hold.
    if (health?.unauthorized) return undefined

    return global.serverCtx(key) ?? previous
  }, undefined)

  const pane = createMemo(() => {
    const value = ctx()

    if (!value) {
      return ServerStatusPane({
        state: pendingState(global.servers.health[key]),
        onRetry: () => void global.servers.retry(),
      })
    }

    return [
      createComponent(context.provider, {
        conn: props.conn,
        ctx: value,
        get children() {
          return props.children
        },
      }),
      createUnreachableBanner(
        () => pendingState(global.servers.health[key]) === "unreachable",
        () => void global.servers.retry(),
      ),
    ]
  })

  // Solid supports a component returning an accessor; the JSX.Element annotation is the public type every call site
  // (routes.tsx) compiles against.
  return pane as unknown as JSX.Element
}

export const useData = () => {
  const server = useServer()

  return server.ctx.data
}
