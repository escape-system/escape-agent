import { Route, useParams } from "@solidjs/router"
import { createMemo, lazy, Show, Suspense, type ParentProps } from "solid-js"
import { Home } from "@/home/route"
import { pendingState, readyKey, ServerPendingPane, ServerProvider } from "@/runtime/server/current"
import { useGlobal } from "@/runtime/server/runtime"
import { ServerConnection, useServers } from "@/runtime/server/registry"
import type { ServerHealth } from "@/runtime/server/health"
import { SessionPanelFrame, SessionRouteFrame } from "@/session/session-frame"
import { LayoutProvider } from "@/shell/state/layout"
import { SettingsSurfaceProvider } from "@/settings/surface"
import Shell from "@/shell/shell"
import { requireServerKey } from "./session"
import { PluginAttachment } from "@/runtime/plugin/root"

export const File = lazy(() => import("@opencode/session-ui/file").then((module) => ({ default: module.File })))

const loadSessionRoute = () => Promise.all([import("@/session/route"), File.preload()]).then(([module]) => module)

const DraftRoute = lazy(() => import("@/new-session/route").then((module) => ({ default: module.DraftRoute })))

const SettingsScreen = lazy(() => import("@/settings/shell").then((module) => ({ default: module.SettingsScreen })))

const ConnectServerScreen = lazy(() =>
  import("@/servers/connect/screen").then((module) => ({ default: module.ConnectServerScreen })),
)

// The dev server and Beta only. Both are fixed at build time, so other builds, prod included, drop the import and never
// contain the lab, its stories or knobs. Beta loads it as its own chunk, only when /components opens.
const ComponentsLab =
  import.meta.env.DEV || import.meta.env.VITE_OPENCODE_CHANNEL === "beta"
    ? lazy(() => import("@/components-lab/lab"))
    : undefined

const TargetSessionRouteContent = lazy(() =>
  loadSessionRoute().then((module) => ({ default: module.TargetSessionRouteContent })),
)

export function preloadRoute(url: string) {
  const pathname = url.split(/[?#]/, 1)[0]

  if (pathname === "/new-session") return DraftRoute.preload().then(() => undefined)

  if (pathname === "/settings") return SettingsScreen.preload().then(() => undefined)

  if (/^\/server\/[^/]+\/session\/[^/]+$/.test(pathname))
    return TargetSessionRouteContent.preload().then(() => undefined)

  return Promise.resolve()
}

export function AppRoutes() {
  return (
    <Route component={AppLayout}>
      <Route path="/" component={Home} />
      <Route path="/settings" component={SettingsScreen} />
      <Route
        path="/server/:serverKey/session/:id"
        component={() => (
          <SessionRouteFrame>
            <Suspense
              fallback={
                <div class="flex min-h-0 flex-1 px-[var(--shell-inline-inset,8px)] pb-[var(--shell-bottom-inset,8px)] pt-[var(--shell-top-inset,8px)]">
                  <SessionPanelFrame raised />
                </div>
              }
            >
              <TargetServerRoute>
                <TargetSessionRouteContent />
              </TargetServerRoute>
            </Suspense>
          </SessionRouteFrame>
        )}
      />
      <Route path="/new-session" component={DraftRoute} />
      {ComponentsLab && <Route path="/components" component={ComponentsLab} />}
    </Route>
  )
}

function TargetServerRoute(props: ParentProps) {
  const params = useParams<{ serverKey: string }>()
  const global = useGlobal()

  const connection = createMemo(() =>
    global.servers.list().find((item) => ServerConnection.key(item) === requireServerKey(params.serverKey)),
  )

  return (
    <Show when={connection()} keyed>
      {(connection) => <ServerProvider conn={connection}>{props.children}</ServerProvider>}
    </Show>
  )
}

function AppLayout(props: ParentProps) {
  const servers = useServers()
  const global = useGlobal()

  // A lone server that rejects our credentials (e.g. the web app before pairing) has nothing else to show.
  const signedOut = () => {
    const only = servers.list.length === 1 ? servers.list[0] : undefined

    if (only?.type !== "http") return

    return global.servers.health[ServerConnection.key(only)]?.unauthorized ? only : undefined
  }

  // The server a lone/deep-linked app is showing: the canonical one when listed, else the first.
  const activeConnection = () => servers.list.find((item) => servers.current(item)) ?? servers.list[0]

  // `undefined` covers both "no server yet" and "the first probe has not answered": the shell must never be blank.
  const activeHealth = (): ServerHealth | undefined => {
    const conn = activeConnection()

    return conn ? global.servers.health[ServerConnection.key(conn)] : undefined
  }

  const activeKey = () => {
    const conn = activeConnection()

    return conn ? ServerConnection.key(conn) : undefined
  }

  // Latch: a server that already answered stays "ready" through later failed polls, so a transient outage does not tear
  // the mounted session out of the shell (which would reset its scroll). The route's own unreachable panel explains it.
  const ready = createMemo<string | undefined>(
    (previous) => readyKey(activeHealth(), activeKey(), previous),
    undefined,
  )

  const connecting = () => {
    const health = activeHealth()

    // A rejected credential has its own screen, and a version mismatch its own panel inside the session route.
    if (health?.unauthorized || health?.incompatible) return false

    return !ready()
  }

  return (
    <Show when={servers.list.length > 0 && !signedOut()} fallback={<ConnectServerScreen url={signedOut()?.http.url} />}>
      <LayoutProvider>
        <SettingsSurfaceProvider>
          <PluginAttachment>
            <Shell>
              <Show when={!connecting()} fallback={<ShellSkeleton />}>{props.children}</Show>
            </Shell>
          </PluginAttachment>
        </SettingsSurfaceProvider>
      </LayoutProvider>
    </Show>
  )
}

// Shown in the shell's main region while the active server has not reported healthy yet. It keeps the chrome (titlebar,
// sidebar) mounted so the app never collapses to an empty document during the first health probe.
export function ShellSkeleton() {
  const global = useGlobal()
  const servers = useServers()

  const state = () => {
    const conn = servers.list.find((item) => servers.current(item)) ?? servers.list[0]

    return pendingState(conn ? global.servers.health[ServerConnection.key(conn)] : undefined)
  }

  return (
    <div data-testid="shell-skeleton" class="flex min-h-0 flex-1 flex-col p-[var(--shell-inline-inset,8px)]">
      <div class="flex min-h-0 flex-1 flex-col overflow-hidden rounded-lg border border-white/10 bg-v2-background-bg-base">
        {/* The pane is plain DOM, so it owns its own state swap; calling ServerStatusPane inline would freeze it. */}
        <ServerPendingPane state={state} onRetry={() => void global.servers.retry()} />
      </div>
    </div>
  )
}
