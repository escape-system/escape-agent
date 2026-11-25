import type { SubagentNode, SubagentTree } from "./agent-wire"
import type { Fetched } from "./agent-queries"

/**
 * Decisions the subagent tree strip makes before it renders (`docs/13-ui.md`).
 *
 * They live here, not inside the component, so a unit test can prove them: under
 * `--conditions=solid` the `solid-js/web` entry resolves to Solid's SERVER build,
 * where `render`/`createEffect` are no-ops, so a rendered-Solid test would pass
 * vacuously instead of proving anything. The pure functions are what the
 * component branches on.
 */

/** Where the user's explicit open/closed choice is stored. */
export const SUBAGENT_TREE_STORAGE_KEY = "opencode.session.subagent-tree.v3.collapsed"

/**
 * The key the previous version wrote. It only ever held an explicit choice (the old code wrote
 * on toggle, never on mount), so reading it keeps a user who collapsed the strip collapsed
 * without forcing everyone else back to collapsed.
 */
export const LEGACY_SUBAGENT_TREE_STORAGE_KEY = "opencode.session.subagent-tree.v2.collapsed"

/**
 * Whether the strip starts closed. Absent any stored choice it starts OPEN: the desktop header
 * is the only place that lists every subagent, and a collapsed strip there is the reported bug.
 */
export function readSubagentTreeCollapsed(
  storage: Pick<Storage, "getItem"> | undefined = typeof localStorage === "undefined" ? undefined : localStorage,
): boolean {
  if (!storage) return false
  const current = storage.getItem(SUBAGENT_TREE_STORAGE_KEY)
  if (current !== null) return current === "true"
  const legacy = storage.getItem(LEGACY_SUBAGENT_TREE_STORAGE_KEY)
  if (legacy !== null) return legacy === "true"

  return false
}

/** Records an explicit choice so it survives the next mount. */
export function writeSubagentTreeCollapsed(
  collapsed: boolean,
  storage: Pick<Storage, "setItem"> | undefined = typeof localStorage === "undefined" ? undefined : localStorage,
) {
  storage?.setItem(SUBAGENT_TREE_STORAGE_KEY, String(collapsed))
}

/**
 * The tree's nodes without its root. The root is the family's own session, not a subagent, so
 * every count and badge in the strip and in the mobile sheet describes these nodes only.
 *
 * `rootID` comes from the response; when a server omits it the root is the first node of the
 * server's DFS order, so the count falls back to `nodes.length - 1`.
 */
export function subagentNodes(nodes: readonly SubagentNode[], rootID?: string): readonly SubagentNode[] {
  if (!rootID) return nodes.slice(1)

  return nodes.filter((node) => node.id !== rootID)
}

/** How many subagents the tree holds: the family total excluding its root. */
export function subagentCount(nodes: readonly SubagentNode[], rootID?: string): number {
  return subagentNodes(nodes, rootID).length
}

/** Subagents in the given states, with the family root excluded like every other count here. */
export function subagentCountIn(
  nodes: readonly SubagentNode[],
  states: readonly SubagentNode["state"][],
  rootID?: string,
): number {
  return subagentNodes(nodes, rootID).filter((node) => states.includes(node.state)).length
}

/**
 * How the strip explains a missing list. Presence means "render the line"; `detail` is the raw
 * reason, exposed as a `data-` attribute rather than text so no untranslated server string is
 * printed in a localized surface.
 */
export type SubagentTreeNotice = { detail?: string }

/**
 * Why the strip has no list to show, or `undefined` when it has one. A tree that is not loaded
 * yet, or a session with no subagents, explains nothing. A request the server refused
 * (401/403/503) or that failed outright does, so the surface never disappears in silence.
 */
export function subagentTreeNotice(input: {
  isError?: boolean
  error?: unknown
  data?: Fetched<SubagentTree>
}): SubagentTreeNotice | undefined {
  if (input.isError) {
    const message = input.error instanceof Error ? input.error.message.trim() : ""

    return message ? { detail: message } : {}
  }
  const data = input.data
  if (!data || data.available) return undefined
  // "no session" is the panel's own placeholder, not a failure the user should read.
  const reason = data.reason.trim()
  if (!reason || reason === "no session") return undefined

  return { detail: reason }
}
