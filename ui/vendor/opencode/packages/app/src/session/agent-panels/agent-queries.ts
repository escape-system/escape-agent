import { useQuery } from "@tanstack/solid-query"
import { serverMessage, useAgenticRequest } from "@/settings/agentic/config"
import { useServerSDK } from "@/runtime/server/client"
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
 * Reads one experimental route. Every route here answers `{data: …}`, so the
 * envelope is unwrapped once: a body without it is a malformed response, not an
 * empty list.
 */
async function read(request: Request, path: string, signal: AbortSignal | undefined): Promise<ReadResult> {
  const response = await request(path, { signal })
  if (response.status === 503) return { kind: "unavailable", reason: await serverMessage(response) }
  if (!response.ok) throw new Error(await serverMessage(response))

  const body: unknown = await response.json()
  if (!body || typeof body !== "object" || !("data" in body)) throw new Error(`Malformed response from ${path}`)

  return { kind: "data", value: body.data }
}

// The panels share these keys, so a screen with the tree, the observer rows and
// the composer chip issues each request once.
export function useSubagentTreeQuery(sessionID: () => string | undefined) {
  const sdk = useServerSDK()
  const request = useAgenticRequest()

  return useQuery(() => ({
    queryKey: [sdk.scope, "agent-panels", "tree", sessionID()],
    queryFn: async ({ signal }): Promise<Fetched<SubagentTree>> => {
      const id = sessionID()
      if (!id) return { available: false, reason: "no session" }

      const result = await read(request, `/api/experimental/subagent/tree?sessionID=${encodeURIComponent(id)}`, signal)
      if (result.kind === "unavailable") return { available: false, reason: result.reason }
      const tree = parseSubagentTree(result.value)

      return tree ? { available: true, value: tree } : { available: false, reason: "malformed tree" }
    },
    enabled: !!sessionID() && sdk.connection.status() === "connected",
    refetchOnMount: false,
    staleTime: 30_000,
    retry: false,
  }))
}

export function useObserverRepairsQuery(sessionID: () => string | undefined) {
  const sdk = useServerSDK()
  const request = useAgenticRequest()

  return useQuery(() => ({
    queryKey: [sdk.scope, "agent-panels", "repairs", sessionID()],
    queryFn: async ({ signal }): Promise<Fetched<ObserverRepair[]>> => {
      const id = sessionID()
      if (!id) return { available: false, reason: "no session" }

      const result = await read(
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

      const result = await read(request, `/api/experimental/observer?sessionID=${encodeURIComponent(id)}`, signal)
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

      const result = await read(request, `/api/experimental/goal?sessionID=${encodeURIComponent(id)}`, signal)
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
      const result = await read(request, "/api/experimental/lane", signal)
      if (result.kind === "unavailable") return { available: false, reason: result.reason }

      return { available: true, value: parseLaneStatuses(result.value) }
    },
    enabled: sdk.connection.status() === "connected",
    refetchOnMount: false,
    staleTime: 30_000,
    retry: false,
  }))
}
