import { matchesModelSearch } from "@/providers/models/search"
import type { RegistryModel } from "./api"

/** The server caps one fallback chain at 16 entries (`MAX_CHAIN` in `models_registry.rs`). */
export const MAX_CHAIN = 16

export const chainRef = (model: Pick<RegistryModel, "providerID" | "id">) => `${model.providerID}/${model.id}`

/** Appends a ref, unless it is already present or the chain is full. Never mutates the input. */
export function addToChain(chain: readonly string[], ref: string): string[] {
  if (!ref || chain.includes(ref) || chain.length >= MAX_CHAIN) return [...chain]

  return [...chain, ref]
}

/** Drops every occurrence of `ref`. Never mutates the input. */
export function removeFromChain(chain: readonly string[], ref: string): string[] {
  return chain.filter((item) => item !== ref)
}

/** Moves the entry at `index` by `delta` places; out-of-range moves and indices are a no-op. */
export function applyChainMove(chain: readonly string[], index: number, delta: number): string[] {
  const next = [...chain]
  const target = index + delta

  if (!Number.isInteger(index) || !Number.isInteger(delta)) return next
  if (index < 0 || index >= next.length) return next
  if (target < 0 || target >= next.length) return next
  const [item] = next.splice(index, 1)

  next.splice(target, 0, item)

  return next
}

const searchable = (model: RegistryModel) => [model.name, model.id, model.providerID, ...(model.variants ?? [])]

/**
 * Splits the registry into models that can be appended and models already in the chain.
 *
 * A query filters both lists by `[name, id, providerID, ...variants]` (case- and
 * punctuation-insensitive, via `matchesModelSearch`); a chained model is never
 * dropped *because* it is chained — it lands in `alreadyAdded` so the caller can
 * show it as present instead of leaving a blank result box. An empty query lists
 * every model. An empty registry yields two empty lists, which the caller renders
 * as an explicit empty state.
 */
export function chainCandidates(
  models: readonly RegistryModel[],
  chain: readonly string[],
  query: string,
): { candidates: RegistryModel[]; alreadyAdded: RegistryModel[] } {
  const added = new Set(chain)
  const matched = models.filter((model) => matchesModelSearch(query.trim(), searchable(model)))
  const sorted = [...matched].sort((a, b) => a.name.localeCompare(b.name) || chainRef(a).localeCompare(chainRef(b)))

  return {
    candidates: sorted.filter((model) => !added.has(chainRef(model))),
    alreadyAdded: sorted.filter((model) => added.has(chainRef(model))),
  }
}

/** One rendered row of the add-model listbox: a candidate, or a chained model shown disabled. */
export type ChainSearchRow = {
  ref: string
  model: RegistryModel
  disabled: boolean
  group: "candidate" | "added"
}

/** Candidate rows the listbox renders before it asks the user to narrow the query. */
export const CHAIN_SEARCH_LIMIT = 9

/** Everything the add-model listbox renders and navigates. Derived, never stored. */
export type ChainSearchState = {
  rows: ChainSearchRow[]
  /** Index into `rows`, or `-1` when nothing matches. */
  activeIndex: number
  active: ChainSearchRow | undefined
  /** Every match, already-added refs included. */
  total: number
  /** Matches the render cap hides, for the "showing X of Y" hint. */
  hidden: number
  /** Rows `Enter` can actually add. */
  enabled: number
}

/**
 * Folds the add-model search into the exact list the listbox shows.
 *
 * Candidates come first in name order, already-chained matches last (disabled,
 * still searchable). The candidate side is capped at `limit` rows so a broad
 * query never renders the whole registry; `total`/`hidden` let the view say how
 * many matches it is holding back. `activeIndex` clamps into the rendered rows,
 * so navigation can never point at a row that is not on screen.
 */
export function searchState(input: {
  candidates: readonly RegistryModel[]
  alreadyAdded: readonly RegistryModel[]
  activeIndex?: number
  limit?: number
}): ChainSearchState {
  const limit = Math.max(1, Math.trunc(input.limit ?? CHAIN_SEARCH_LIMIT))
  const shown = input.candidates.slice(0, limit)
  const rows: ChainSearchRow[] = [
    ...shown.map((model) => ({ ref: chainRef(model), model, disabled: false, group: "candidate" as const })),
    ...input.alreadyAdded.map((model) => ({ ref: chainRef(model), model, disabled: true, group: "added" as const })),
  ]
  const requested = input.activeIndex
  const activeIndex =
    rows.length === 0
      ? -1
      : Math.min(Math.max(Number.isFinite(requested) ? Math.trunc(requested!) : 0, 0), rows.length - 1)

  return {
    rows,
    activeIndex,
    active: rows[activeIndex],
    total: input.candidates.length + input.alreadyAdded.length,
    hidden: Math.max(0, input.candidates.length - shown.length),
    enabled: shown.length,
  }
}

export type ChainSearchCommand = "next" | "previous" | "first" | "last" | "enter"

export type ChainSearchOutcome = {
  activeIndex: number
  /** The ref `Enter` adds, when the active row can be added. */
  add?: string
  /** Why the active row refused `Enter`: already in the chain, or the chain is full. */
  blocked?: "already-added" | "cap"
}

/**
 * What a key does to the add-model listbox. Arrows wrap over the *rendered*
 * rows (so the active row is always on screen), Home/End jump to the ends, and
 * `Enter` either returns the ref to append or says why it refused.
 */
export function searchKey(
  state: ChainSearchState,
  command: ChainSearchCommand,
  chainLength = 0,
): ChainSearchOutcome {
  const count = state.rows.length

  if (count === 0) return { activeIndex: -1 }

  const at = state.activeIndex < 0 ? 0 : state.activeIndex

  if (command === "first") return { activeIndex: 0 }
  if (command === "last") return { activeIndex: count - 1 }
  if (command === "next") return { activeIndex: (at + 1) % count }
  if (command === "previous") return { activeIndex: at <= 0 ? count - 1 : at - 1 }

  const row = state.rows[at]

  if (!row) return { activeIndex: at }
  if (row.disabled) return { activeIndex: at, blocked: "already-added" }
  if (chainLength >= MAX_CHAIN) return { activeIndex: at, blocked: "cap" }

  return { activeIndex: at, add: row.ref }
}

export type HighlightRange = { start: number; end: number }

const isWordChar = (char: string) => /[\p{Letter}\p{Number}]/u.test(char)

/** The same normalization `matchesModelSearch` applies: letters and numbers, single spaces. */
const normalizeQuery = (value: string) =>
  value
    .toLowerCase()
    .replace(/[^\p{Letter}\p{Number}]+/gu, " ")
    .trim()
    .replace(/\s+/g, " ")

type Projection = { normalized: string; compact: string; normalizedMap: number[]; compactMap: number[] }

/**
 * Projects a value onto its search-normalized form while keeping the offset of
 * every normalized character in the original string. Lower-casing changes the
 * length of a few scripts, and then offsets no longer line up, so that value is
 * not highlightable.
 */
function project(value: string): Projection | undefined {
  const lowered = value.toLowerCase()

  if (lowered.length !== value.length) return undefined
  const normalizedMap: number[] = []
  const compactMap: number[] = []
  let normalized = ""
  let compact = ""
  let gap = -1

  for (let index = 0; index < lowered.length; index += 1) {
    const char = lowered[index]!

    if (isWordChar(char)) {
      if (gap >= 0) {
        normalizedMap.push(gap)
        normalized += " "
      }
      gap = -1
      normalizedMap.push(index)
      normalized += char
      compactMap.push(index)
      compact += char
      continue
    }

    // Only a separator that sits between two words becomes a space; edges are dropped.
    if (normalized.length > 0 && gap < 0) gap = index
  }

  return { normalized, compact, normalizedMap, compactMap }
}

function occurrences(haystack: string, needle: string, map: number[]): HighlightRange[] {
  if (!needle || !haystack) return []
  const ranges: HighlightRange[] = []
  let at = haystack.indexOf(needle)

  while (at !== -1) {
    const start = map[at]
    const last = map[at + needle.length - 1]

    if (start !== undefined && last !== undefined) ranges.push({ start, end: last + 1 })
    at = haystack.indexOf(needle, at + needle.length)
  }

  return ranges
}

/**
 * The spans of `value` the model search matched, for rendering `<mark>`s.
 *
 * The search is case- and punctuation-insensitive, so a plain `indexOf` cannot
 * find the span; the query is matched against the same normalized projection
 * the filter uses, both with separators and compacted, and the offsets are
 * mapped back onto the original text. Overlaps merge, so a value never gets
 * nested marks.
 */
export function highlightRanges(value: string, query: string): HighlightRange[] {
  const needle = normalizeQuery(query)

  if (!needle) return []
  const projection = project(value)

  if (!projection) return []
  // The compact pass matters even for a single-word query: the *value* may carry
  // separators the query left out ("glm53" against "GLM 5.3").
  const compactNeedle = needle.replaceAll(" ", "")
  const found = [
    ...occurrences(projection.normalized, needle, projection.normalizedMap),
    ...occurrences(projection.compact, compactNeedle, projection.compactMap),
  ].sort((a, b) => a.start - b.start || a.end - b.end)
  const merged: HighlightRange[] = []

  for (const range of found) {
    const previous = merged[merged.length - 1]

    if (previous && range.start <= previous.end) previous.end = Math.max(previous.end, range.end)
    else merged.push({ ...range })
  }

  return merged
}

export type HealthUnit = "second" | "minute" | "hour" | "day"

/**
 * How long a temporary exclusion still lasts, split into a whole amount and a
 * unit so the view can format it with `Intl.RelativeTimeFormat`.
 *
 * Expired or unparseable timestamps yield `undefined` (nothing left to show).
 * Amounts round up, so a badge never says "0 minutes".
 */
export function healthRemaining(untilMs: number, nowMs: number): { value: number; unit: HealthUnit } | undefined {
  if (!Number.isFinite(untilMs) || !Number.isFinite(nowMs)) return undefined
  const seconds = Math.ceil((untilMs - nowMs) / 1000)

  if (seconds <= 0) return undefined
  if (seconds < 60) return { value: seconds, unit: "second" }
  if (seconds < 3600) return { value: Math.ceil(seconds / 60), unit: "minute" }
  if (seconds < 86_400) return { value: Math.ceil(seconds / 3600), unit: "hour" }

  return { value: Math.ceil(seconds / 86_400), unit: "day" }
}

/**
 * Whether a model is listed at all. An empty chain means "no restriction"; a
 * non-empty chain is a *routing* preference (the fallback order), never a
 * visibility allow-list, so it does not exclude anything here either.
 *
 * The Models tab's per-model switch and the registry's `unavailable` flag remain
 * the only exclusions.
 */
export function isVisibleByPolicy(
  model: { hidden: boolean; unavailable: boolean },
  _routingChain: readonly string[],
  _ref: string,
): boolean {
  return !model.hidden && !model.unavailable
}
