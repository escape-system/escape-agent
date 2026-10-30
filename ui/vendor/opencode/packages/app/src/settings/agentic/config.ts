import { createEffect, createMemo, createSignal } from "solid-js"
import { useQuery, useQueryClient } from "@tanstack/solid-query"
import { authTokenFromCredentials } from "@/runtime/server/api"
import { useServerSDK } from "@/runtime/server/client"

export type ConfigObject = Record<string, unknown>

export type AgenticSection = "memory" | "goal" | "observer" | "subagents" | "lanes"

const SECTIONS: readonly AgenticSection[] = ["memory", "goal", "observer", "subagents", "lanes"]

type SavedSections = Partial<Record<AgenticSection, ConfigObject>>

export function isConfigObject(value: unknown): value is ConfigObject {
  return !!value && typeof value === "object" && !Array.isArray(value)
}

/** Deep-merges config objects; later values win, arrays and scalars replace. */
export function mergeConfig(base: ConfigObject, next: ConfigObject): ConfigObject {
  const result: ConfigObject = { ...base }

  for (const [key, value] of Object.entries(next)) {
    const existing = result[key]
    result[key] = isConfigObject(existing) && isConfigObject(value) ? mergeConfig(existing, value) : value
  }

  return result
}

/** The documented precedence: each later document overrides the previous one. Display only. */
export function mergeDocuments(entries: readonly unknown[]): ConfigObject {
  return entries.reduce<ConfigObject>((merged, entry) => {
    if (!isConfigObject(entry) || entry.type !== "document" || !isConfigObject(entry.info)) return merged

    return mergeConfig(merged, entry.info)
  }, {})
}

/** The server's error body is `{ _tag, message }`; fall back to the status line. */
export async function serverMessage(response: Response) {
  try {
    const body: unknown = await response.json()

    if (isConfigObject(body)) {
      if (typeof body.message === "string" && body.message) return body.message

      const data = body.data

      if (isConfigObject(data) && typeof data.message === "string" && data.message) return data.message
    }
  } catch {
    // Not JSON.
  }

  return `${response.status} ${response.statusText}`.trim()
}

/** Raw `/api/*` access for the experimental routes the typed SDK does not model yet. */
export function useAgenticRequest() {
  const sdk = useServerSDK()

  return async (path: string, init?: RequestInit) => {
    const base = sdk.http.url.endsWith("/") ? sdk.http.url : `${sdk.http.url}/`
    const headers = new Headers(init?.headers)

    if (sdk.http.password) {
      headers.set("Authorization", `Basic ${authTokenFromCredentials({ password: sdk.http.password })}`)
    }

    if (init?.body !== undefined) headers.set("Content-Type", "application/json")

    return fetch(new URL(path.replace(/^\//, ""), base), { ...init, headers })
  }
}

export interface AgenticConfig {
  loading(): boolean
  failed(): boolean
  error(): string | undefined
  saving(): boolean
  restart(): boolean
  display(): ConfigObject
  section(key: AgenticSection): ConfigObject
  save(key: AgenticSection, changes: ConfigObject): Promise<boolean>
  clearError(): void
  reload(): void
}

/**
 * Sections this window has written, per server. The server's loaded config
 * snapshot does not change until a restart, so without this a page remount
 * would save against the pre-write document and drop the earlier keys.
 *
 * They are mirrored to `sessionStorage` so a reload before the restart keeps
 * showing the written values instead of silently reverting to the snapshot.
 */
const savedSections = new Map<string, SavedSections>()

function storageKey(scope: string) {
  return `settings.agentic.pending.${encodeURIComponent(scope)}`
}

function loadSaved(scope: string): SavedSections {
  try {
    const raw = sessionStorage.getItem(storageKey(scope))

    if (!raw) return {}
    const parsed: unknown = JSON.parse(raw)

    if (!isConfigObject(parsed)) return {}
    const result: SavedSections = {}

    for (const key of SECTIONS) {
      const value = parsed[key]

      if (isConfigObject(value)) result[key] = value
    }

    return result
  } catch {
    return {}
  }
}

function storeSaved(scope: string, sections: SavedSections) {
  try {
    sessionStorage.setItem(storageKey(scope), JSON.stringify(sections))
  } catch {
    // Storage is optional: the in-memory overlay still covers SPA navigation.
  }
}

/** True when every value in `overlay` is already present in `base`. */
function contains(base: ConfigObject, overlay: ConfigObject): boolean {
  return Object.entries(overlay).every(([key, value]) => {
    const existing = base[key]

    if (isConfigObject(value)) return isConfigObject(existing) && contains(existing, value)

    return existing === value
  })
}

/**
 * The additive config sections (`memory`, `goal`, `observer`, `subagents`,
 * `lanes`) read from the config documents and written one section at a time.
 *
 * The server replaces a whole top-level key, so a save merges the section's
 * current value first. The loaded snapshot only changes after a restart, which
 * the PATCH response announces, so the written values stay layered over the
 * fetched documents until the page reloads.
 */
export function createAgenticConfig(): AgenticConfig {
  const sdk = useServerSDK()
  const client = useQueryClient()
  const request = useAgenticRequest()

  const query = useQuery(() => ({
    queryKey: [sdk.scope, "settings-agentic-config"],
    queryFn: () => sdk.api.config.get(),
    staleTime: 30_000,
  }))

  const initial = savedSections.get(sdk.scope) ?? loadSaved(sdk.scope)
  savedSections.set(sdk.scope, initial)

  const [written, setWritten] = createSignal<SavedSections>(initial)
  const [restart, setRestart] = createSignal(Object.keys(initial).length > 0)
  const [error, setError] = createSignal<string>()
  const [saving, setSaving] = createSignal(false)

  const display = createMemo(() => mergeDocuments(query.data ?? []))

  // A restarted server's snapshot already has these values, so they are no
  // longer pending and the restart notice can clear.
  createEffect(() => {
    const data = query.data

    if (!data) return
    const document = mergeDocuments(data)
    const current = written()
    const remaining: SavedSections = {}
    let settled = false

    for (const key of SECTIONS) {
      const value = current[key]

      if (!value) continue
      const base = document[key]

      if (isConfigObject(base) && contains(base, value)) {
        settled = true

        continue
      }

      remaining[key] = value
    }

    if (!settled) return
    setWritten(remaining)
    savedSections.set(sdk.scope, remaining)
    storeSaved(sdk.scope, remaining)

    if (Object.keys(remaining).length === 0) setRestart(false)
  })

  const section = (key: AgenticSection): ConfigObject => {
    const document = display()[key]

    return mergeConfig(isConfigObject(document) ? document : {}, written()[key] ?? {})
  }

  const save = async (key: AgenticSection, changes: ConfigObject) => {
    setSaving(true)
    setError(undefined)

    try {
      const merged = mergeConfig(section(key), changes)
      const response = await request("/api/experimental/config", {
        method: "PATCH",
        body: JSON.stringify({ [key]: merged }),
      })

      if (!response.ok) {
        setError(await serverMessage(response))

        return false
      }

      // A 204 has no body, but draining the response keeps Chromium from reporting the
      // stream as aborted when the caller only reads headers.
      await response.arrayBuffer()
      setWritten((current) => {
        const next = { ...current, [key]: merged }
        savedSections.set(sdk.scope, next)
        storeSaved(sdk.scope, next)

        return next
      })
      setRestart(response.headers.get("x-opencode-restart") === "required")
      void client.invalidateQueries({ queryKey: [sdk.scope, "settings-agentic-config"] })

      return true
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))

      return false
    } finally {
      setSaving(false)
    }
  }

  return {
    loading: () => query.isPending,
    failed: () => query.isError,
    error,
    saving,
    restart,
    display,
    section,
    save,
    clearError: () => setError(undefined),
    reload: () => void query.refetch(),
  }
}
