import { onCleanup } from "solid-js"
import { useQuery } from "@tanstack/solid-query"
import { serverMessage, useAgenticRequest } from "@/settings/agentic/config"
import { useServerSDK } from "@/runtime/server/client"
import { useServer } from "@/runtime/server/current"
import {
  parseGoal,
  parseLaneStatuses,
  parseObserverRepairs,
  parseObserverState,
  parseSubagentTree,
  type Goal,
  type LaneStatus,
  type ObserverBudget,
  type ObserverRepair,
  type SubagentTree,
} from "./agent-wire"

/**
 * A fetched extension value. A `503` is the server saying the subsystem is not
 * attached; it is shown inline instead of being retried or rendered as empty.
 */
export type Fetched<T> = { available: true; value: T } | { available: false; reason: string }

type Request = (path: string, init?: RequestInit) => Promise<Response>

type ReadResult = { kind: "unavailable"; reason: string } | { kind: "data"; value: unknown }

/**
 * Statuses that mean "this surface cannot be read right now", not "the request failed".
 * `503` is a detached subsystem; `401`/`403` are a server this tab is not authorized for yet
 * (the live server is auth-gated, so a desktop strip answered with 401 used to vanish with no
 * message at all). Both become an inline, explainable `unavailable`.
 */
const unavailableStatuses = new Set([401, 403, 503])

/**
 * Reads one experimental route. Every route here answers `{data: …}`, so the
 * envelope is unwrapped once: a body without it is a malformed response, not an
 * empty list.
 */
export async function readExperimental(
  request: Request,
  path: string,
  signal: AbortSignal | undefined,
): Promise<ReadResult> {
  const response = await request(path, { signal })
  if (unavailableStatuses.has(response.status))
    return { kind: "unavailable", reason: await serverMessage(response) }
  if (!response.ok) throw new Error(await serverMessage(response))

  const body: unknown = await response.json()
  if (!body || typeof body !== "object" || !("data" in body)) throw new Error(`Malformed response from ${path}`)

  return { kind: "data", value: body.data }
}

// The panels share these keys, so a screen with the tree, the observer rows and
// the composer chip issues each request once.
export function useSubagentTreeQuery(sessionID: () => string | undefined) {
  const sdk = useServerSDK()
  const server = useServer()
  const request = useAgenticRequest()

  const query = useQuery(() => ({
    queryKey: [sdk.scope, "agent-panels", "tree", sessionID()],
    queryFn: async ({ signal }): Promise<Fetched<SubagentTree>> => {
      const id = sessionID()
      if (!id) return { available: false, reason: "no session" }

      const result = await readExperimental(
        request,
        `/api/experimental/subagent/tree?sessionID=${encodeURIComponent(id)}`,
        signal,
      )
      if (result.kind === "unavailable") return { available: false, reason: result.reason }
      const tree = parseSubagentTree(result.value)

      return tree ? { available: true, value: tree } : { available: false, reason: "malformed tree" }
    },
    enabled: !!sessionID() && sdk.connection.status() === "connected",
    // SSE events drive refreshes. An unconditional 2 s poll re-resolves tree.data
    // twice a second, which re-renders the timeline's sticky header and makes the
    // composer flicker while typing.
    refetchOnMount: true,
    // The tree only changes on a status/execution event, and each one triggers a
    // trailing refetch. Two seconds keeps a remount from asking again while the
    // previous answer is still the newest the server has.
    staleTime: 2_000,
    retry: false,
  }))

  const eventSession = (raw: { data?: unknown }) => {
    if (!raw.data || typeof raw.data !== "object") return undefined
    const data = raw.data as { sessionID?: unknown; id?: unknown }

    return typeof data.sessionID === "string"
      ? data.sessionID
      : typeof data.id === "string"
        ? data.id
        : undefined
  }
  const knownNodes = () => new Set((query.data?.available ? query.data.value.nodes : []).map((node) => node.id))
  const belongsToCurrent = (candidate: string, current: string) => {
    let cursor: string | undefined = candidate
    const visited = new Set<string>()
    while (cursor && !visited.has(cursor)) {
      if (cursor === current || knownNodes().has(cursor)) return true
      visited.add(cursor)
      cursor = server.ctx.data.session.get(cursor)?.parentID
    }
    return false
  }
  const related = (raw: { type?: string; data?: unknown }, current: string) => {
    const source = eventSession(raw)
    if (!source || source === current) return true
    if (knownNodes().has(source)) return true
    const family = server.ctx.data.session.family(current)
    if (family.includes(source)) return true
    if (belongsToCurrent(source, current)) return true
    if (raw.data && typeof raw.data === "object") {
      const parentID = (raw.data as { parentID?: unknown }).parentID
      if (
        typeof parentID === "string" &&
        (parentID === current ||
          knownNodes().has(parentID) ||
          family.includes(parentID) ||
          belongsToCurrent(parentID, current))
      )
        return true
    }

    return false
  }
  const refetchDebounce = createEventRefetchDebounce(() => void query.refetch())
  const stop = sdk.event.listen((event) => {
    const raw = event as unknown as { type?: string; data?: unknown }
    const current = sessionID()

    if (!current || !raw.type) return
    if (raw.type === "server.connected") {
      refetchDebounce.schedule()
      return
    }
    if (!related(raw, current)) return
    if (
      raw.type === "session.created" ||
      raw.type === "session.deleted" ||
      raw.type === "session.updated" ||
      raw.type === "session.renamed" ||
      raw.type === "session.status" ||
      raw.type.startsWith("session.execution.")
    ) {
      refetchDebounce.schedule()
    }
  })
  onCleanup(() => {
    refetchDebounce.cancel()
    stop()
  })

  return query
}

/**
 * Trailing debounce for the tree's event-driven refetch.
 *
 * A burst of `session.status` / `session.execution.*` events must produce one
 * request, on the trailing edge, not one per event. Each call restarts the
 * timer; `cancel()` clears it on cleanup so a disposed panel cannot refetch.
 * `delayMs` and `setTimeout` are injectable for tests.
 */
export function createEventRefetchDebounce(
  refetch: () => void,
  delayMs = 600,
  timers?: unknown,
) {
  // Structural cast keeps the public shape loose enough for tests to inject a fake clock.
  const clock = (timers ?? { setTimeout, clearTimeout }) as {
    setTimeout(run: () => void, delay: number): ReturnType<typeof setTimeout>
    clearTimeout(id: ReturnType<typeof setTimeout>): void
  }
  let timer: ReturnType<typeof setTimeout> | undefined

  return {
    schedule() {
      if (timer !== undefined) clock.clearTimeout(timer)
      timer = clock.setTimeout(() => {
        timer = undefined
        refetch()
      }, delayMs)
    },
    cancel() {
      if (timer === undefined) return
      clock.clearTimeout(timer)
      timer = undefined
    },
    pending() {
      return timer !== undefined
    },
  }
}

export function useObserverRepairsQuery(sessionID: () => string | undefined) {
  const sdk = useServerSDK()
  const request = useAgenticRequest()

  return useQuery(() => ({
    queryKey: [sdk.scope, "agent-panels", "repairs", sessionID()],
    queryFn: async ({ signal }): Promise<Fetched<ObserverRepair[]>> => {
      const id = sessionID()
      if (!id) return { available: false, reason: "no session" }

      const result = await readExperimental(
        request,
        `/api/experimental/observer/repairs?sessionID=${encodeURIComponent(id)}&limit=20`,
        signal,
      )
      if (result.kind === "unavailable") return { available: false, reason: result.reason }

      return { available: true, value: parseObserverRepairs(result.value) }
    },
    enabled: !!sessionID() && sdk.connection.status() === "connected",
    // Live `observer.repaired` events are merged over these durable rows, so the
    // audit does not need a 3 s poll; one fetch per mount is enough.
    refetchOnMount: false,
    staleTime: 30_000,
    retry: false,
    // The panel lives in the timeline's sticky header. When the subagent tree
    // resolves and `rootID` flips to the tree root, the query key changes. Without
    // this, data blanks out, the panel unmounts, the header shrinks, and the
    // virtualizer re-anchors — the visible "jump to earlier messages" loop.
    placeholderData: (previous: Fetched<ObserverRepair[]> | undefined) => previous,
  }))
}

export function useObserverStateQuery(sessionID: () => string | undefined) {
  const sdk = useServerSDK()
  const request = useAgenticRequest()

  return useQuery(() => ({
    queryKey: [sdk.scope, "agent-panels", "observer", sessionID()],
    queryFn: async ({ signal }): Promise<Fetched<{ budget?: ObserverBudget }>> => {
      const id = sessionID()
      if (!id) return { available: false, reason: "no session" }

      const result = await readExperimental(request, `/api/experimental/observer?sessionID=${encodeURIComponent(id)}`, signal)
      if (result.kind === "unavailable") return { available: false, reason: result.reason }

      return { available: true, value: parseObserverState(result.value) }
    },
    enabled: !!sessionID() && sdk.connection.status() === "connected",
    refetchOnMount: false,
    // Only `observer.escalated` is published; the used/limit counter changes on
    // every repair with no matching event, so this one keeps a slow poll.
    refetchInterval: 15_000,
    staleTime: 30_000,
    retry: false,
  }))
}

export function useSessionGoalQuery(sessionID: () => string | undefined) {
  const sdk = useServerSDK()
  const request = useAgenticRequest()

  return useQuery(() => ({
    queryKey: [sdk.scope, "agent-panels", "goal", sessionID()],
    queryFn: async ({ signal }): Promise<Fetched<Goal | undefined>> => {
      const id = sessionID()
      if (!id) return { available: false, reason: "no session" }

      const result = await readExperimental(request, `/api/experimental/goal?sessionID=${encodeURIComponent(id)}`, signal)
      if (result.kind === "unavailable") return { available: false, reason: result.reason }

      return { available: true, value: parseGoal(result.value) }
    },
    enabled: !!sessionID() && sdk.connection.status() === "connected",
    // `goal.updated` replaces this value live; the fetch is only the initial read.
    refetchOnMount: false,
    staleTime: 30_000,
    retry: false,
  }))
}

export function useLaneSnapshotQuery() {
  const sdk = useServerSDK()
  const request = useAgenticRequest()

  return useQuery(() => ({
    queryKey: [sdk.scope, "agent-panels", "lanes"],
    queryFn: async ({ signal }): Promise<Fetched<LaneStatus[]>> => {
      const result = await readExperimental(request, "/api/experimental/lane", signal)
      if (result.kind === "unavailable") return { available: false, reason: result.reason }

      return { available: true, value: parseLaneStatuses(result.value) }
    },
    enabled: sdk.connection.status() === "connected",
    refetchOnMount: false,
    staleTime: 30_000,
    retry: false,
  }))
}
