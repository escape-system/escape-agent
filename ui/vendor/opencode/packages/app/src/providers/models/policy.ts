import type { RegistryModel } from "@/settings/models/api"

/**
 * The chat picker's policy, as pure functions.
 *
 * When `GET /api/models/registry` answers, the picker follows it: a model is
 * offered when the config does not hide it and its provider is connected. When
 * the registry is unavailable, an older server keeps the store's own visibility
 * rule, so nothing is lost by upgrading.
 *
 * Visibility and ordering are deliberately separate concerns. `models.hidden`
 * is the *only* visibility control besides provider connectivity; a fallback
 * chain never hides anything. `models.chain` / `models.chains.<agent>` are a
 * routing preference, so they only decide where a model sits in the list.
 */

export type RegistryPickerModel = Pick<RegistryModel, "providerID" | "id" | "name" | "visible" | "favorite">

/** `provider/id` with any `#variant` suffix dropped, lowercased and `.` folded to `-`. */
export function normalizeModelRef(reference: string): string {
  const [base = ""] = reference.split("#")

  return base.trim().toLowerCase().replaceAll(".", "-")
}

export const modelReference = (model: { providerID: string; id: string }) => `${model.providerID}/${model.id}`

/**
 * The chain that *orders* the picker for `agent`: its own when `chains` lists
 * one, else the global chain. Ordering cannot exclude a model, so an absent or
 * empty chain simply means "nothing sorts ahead of the rest".
 */
export function effectiveAgentChain(
  chains: Readonly<Record<string, readonly string[]>> | undefined,
  agent: string | undefined,
  chain: readonly string[],
): string[] {
  const own = agent === undefined || chains === undefined ? undefined : chains[agent]

  return [...(own ?? chain)]
}

/** Where `reference` sits in `chain`, or `-1` when absent. A `#variant` entry matches its base ref. */
export function chainIndexOf(chain: readonly string[], reference: string): number {
  const key = normalizeModelRef(reference)

  return chain.findIndex((entry) => normalizeModelRef(entry) === key)
}

/**
 * The picker's visibility inputs.
 *
 * There is intentionally no chain field here: a fallback chain orders rows, it
 * must never remove one, so it cannot be expressed as a visibility question.
 */
export type PickerVisibility = {
  /** The model's `provider/id`. */
  modelRef: string
  /** Its registry entry, when the registry answered and lists it. */
  registryModel?: Pick<RegistryPickerModel, "visible">
  /** Refs the config hides, normalized: the fallback for a model the registry does not list. */
  hiddenSet?: ReadonlySet<string>
  /** `false` excludes the model outright: the picker only offers connected providers. */
  providerConnected?: boolean
  /** The legacy store's answer, read only when the registry does not list the model. */
  legacyVisible?: boolean
}

/**
 * The picker's visibility rule: `!hidden && providerConnected`.
 *
 * The registry's `visible` field is already the inverse of `models.hidden`, so a
 * registry model needs no `hiddenSet`. Without a registry the legacy store's
 * answer stands, which is exactly the old picker. A model absent from a
 * non-empty chain is still visible; chains only reorder.
 */
export function pickerVisible(input: PickerVisibility): boolean {
  if (input.providerConnected === false) return false
  if (input.hiddenSet?.has(normalizeModelRef(input.modelRef))) return false

  if (input.registryModel) return input.registryModel.visible

  return input.legacyVisible !== false
}

export type PickerOrder = { name: string; ref: string; favorite: boolean; chainIndex: number }

/**
 * Favorites first, then the agent's chain order, then the name. A model outside
 * the chain sorts after every chained model; the ref breaks a name tie so two
 * openings of the dialog list the same models in the same order.
 */
export function comparePickerModels(a: PickerOrder, b: PickerOrder): number {
  if (a.favorite !== b.favorite) return a.favorite ? -1 : 1
  const left = a.chainIndex < 0 ? Number.MAX_SAFE_INTEGER : a.chainIndex
  const right = b.chainIndex < 0 ? Number.MAX_SAFE_INTEGER : b.chainIndex

  if (left !== right) return left - right

  return a.name.localeCompare(b.name) || a.ref.localeCompare(b.ref)
}

/** The normalized refs of every favorite the registry reports. */
export function favoriteRefs(
  models: readonly Pick<RegistryPickerModel, "providerID" | "id" | "favorite">[],
): Set<string> {
  return new Set(
    models.filter((model) => model.favorite).map((model) => normalizeModelRef(modelReference(model))),
  )
}
