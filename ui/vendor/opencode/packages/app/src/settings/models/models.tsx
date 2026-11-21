import {
  type Component,
  For,
  Show,
  createEffect,
  createMemo,
  createResource,
  createSignal,
  onCleanup,
  type JSX,
} from "solid-js"
import { Button } from "@opencode/ui/button"
import { Collapsible } from "@opencode/ui/collapsible"
import { Icon } from "@opencode/ui/icon"
import { IconButton } from "@opencode/ui/icon-button"
import { ProviderIcon } from "@opencode/ui/provider-icon"
import { Switch } from "@opencode/ui/switch"
import { TextInput } from "@opencode/ui/text-input"
import { Textarea } from "@opencode/ui/textarea"
import { Schema } from "effect"
import { matchesModelSearch } from "@/providers/models/search"
import { useLanguage } from "@/runtime/i18n/language"
import { Persistence } from "@/runtime/persistence/schema"
import { Persist, persisted } from "@/runtime/persistence/storage"
import { SettingsSearchEmpty } from "@/settings/search-empty"
import { SettingsSearchField } from "@/settings/search-field"
import {
  modelRef,
  ModelsSaveError,
  useAgentOptions,
  useModelsRegistry,
  type AgentOption,
  type ModelQuota,
  type ModelsConfigPatch,
  type RegistryModel,
} from "./api"
import {
  addToChain,
  applyChainMove,
  chainCandidates,
  CHAIN_SEARCH_LIMIT,
  type ChainSearchCommand,
  healthRemaining,
  highlightRanges,
  MAX_CHAIN,
  removeFromChain,
  searchKey,
  searchState,
} from "./chain"

import "@/settings/settings.css"

/**
 * The Models tab is the model *policy* control plane over the frozen wire
 * contract (`GET /api/models/registry`, `PUT /api/config/models`).
 *
 * Left: the catalog — provider groups (collapsed by default, persisted), search,
 * per-model favorite/visibility, cost, notes and temporary unavailability.
 * Right: the agent policy editor — scope, the pinned `agents.<id>.model`, the
 * ordered fallback chain, an add-model search that keeps already-chained refs
 * visible but disabled, the global and per-agent notes, the read-only health
 * list, and finally the quota/usage blocks folded into a collapsed section.
 *
 * Every write sends only the changed key, because the server deep-merges maps
 * and replaces arrays wholesale. There is no restart gate: the server applies
 * the config to the next turn. Optimistic state exists only for the
 * favorite/visibility toggles.
 */

/** Persisted provider-fold state, shared with `runtime/persistence/consumers.test.ts`. */
export const ModelProvidersSchema = Schema.Struct({
  collapsed: Persistence.record(Persistence.fallback(Schema.Boolean, () => false)),
})

type Group = { providerID: string; models: RegistryModel[] }

type CommitInputProps = {
  value: string
  label: string
  placeholder?: string
  numeric?: boolean
  disabled?: boolean
  onCommit: (value: string) => void
}

/** A text/number field that commits on blur or Enter and never sends an untouched value. */
const CommitInput: Component<CommitInputProps> = (props) => {
  const [draft, setDraft] = createSignal(props.value)
  let focused = false

  createEffect(() => {
    const value = props.value

    if (!focused) setDraft(value)
  })

  return (
    <TextInput
      type={props.numeric ? "number" : "text"}
      numeric={props.numeric}
      min={props.numeric ? "0" : undefined}
      step={props.numeric ? "0.01" : undefined}
      class="w-full"
      value={draft()}
      placeholder={props.placeholder}
      aria-label={props.label}
      disabled={props.disabled}
      spellcheck={!props.numeric}
      onInput={(event) => setDraft(event.currentTarget.value)}
      onFocus={() => (focused = true)}
      onBlur={() => {
        focused = false
        const next = draft().trim()

        if (next !== props.value) props.onCommit(next)
      }}
      onKeyDown={(event) => {
        if (event.key === "Enter") event.currentTarget.blur()
      }}
    />
  )
}

type NoteTextareaProps = {
  value: string
  label: string
  placeholder: string
  disabled?: boolean
  onCommit: (value: string) => void
}

/** A free-text note that commits on blur or Cmd/Ctrl+Enter. */
const NoteTextarea: Component<NoteTextareaProps> = (props) => {
  const [draft, setDraft] = createSignal(props.value)
  let focused = false

  createEffect(() => {
    const value = props.value

    if (!focused) setDraft(value)
  })

  const commit = () => {
    const next = draft().trim()

    if (next !== props.value) props.onCommit(next)
  }

  return (
    <Textarea
      rows={2}
      class="!w-full"
      value={draft()}
      aria-label={props.label}
      placeholder={props.placeholder}
      disabled={props.disabled}
      onFocus={() => (focused = true)}
      onInput={(event) => setDraft(event.currentTarget.value)}
      onBlur={() => {
        focused = false
        commit()
      }}
      onKeyDown={(event) => {
        if (event.key !== "Enter" || !(event.metaKey || event.ctrlKey)) return
        event.preventDefault()
        commit()
        event.currentTarget.blur()
      }}
    />
  )
}

const parseNumber = (value: string): number | undefined => {
  if (!value) return undefined
  const parsed = Number(value)

  return Number.isFinite(parsed) && parsed >= 0 ? parsed : undefined
}

/**
 * `text` with the spans the model search matched wrapped in `<mark>`: the search
 * is case- and punctuation-insensitive, so the spans come from `highlightRanges`
 * rather than from `indexOf`.
 */
const Highlighted: Component<{ text: string; query: string }> = (props) => {
  const parts = createMemo(() => {
    const ranges = highlightRanges(props.text, props.query)
    const split: { text: string; match: boolean }[] = []
    let at = 0

    for (const range of ranges) {
      if (range.start > at) split.push({ text: props.text.slice(at, range.start), match: false })
      split.push({ text: props.text.slice(range.start, range.end), match: true })
      at = range.end
    }

    if (at < props.text.length) split.push({ text: props.text.slice(at), match: false })

    return split
  })

  return (
    <For each={parts()}>
      {(part) =>
        part.match ? (
          <mark class="bg-transparent text-v2-text-text-accent">{part.text}</mark>
        ) : (
          <>{part.text}</>
        )
      }
    </For>
  )
}

const clearKey = (overrides: Record<string, boolean>, key: string): Record<string, boolean> => {
  const next = { ...overrides }

  delete next[key]

  return next
}

const invalidModelRef = (message: string): string | undefined => /invalid_model:\s*(\S+)/.exec(message)?.[1]

/**
 * Roles that resolve a model through `models.chains.<role>` but have no agent
 * row in `GET /api/agent`: they are runtime roles, so the policy editor
 * synthesizes them instead of inventing an agent.
 */
const SYSTEM_ROLE_NAMES = ["observer", "scribe"] as const

export const SettingsModels: Component<{
  active?: boolean
  provider?: string
  onReveal?: () => void
  headerAction?: JSX.Element
}> = (props) => {
  const language = useLanguage()
  // The usage rollup is the expensive half of the registry: the first paint
  // asks for the sparse body and the query refetches with usage once the
  // usage section is opened (`docs/15-performance.md`).
  const [usageOpen, setUsageOpen] = createSignal(false)
  const models = useModelsRegistry({ usage: () => usageOpen() })
  const agents = useAgentOptions()

  const [search, setSearch] = createSignal("")
  const [scope, setScope] = createSignal("")
  const [focus, setFocus] = createSignal("")
  const [chainQuery, setChainQuery] = createSignal("")
  // The add-model listbox: one active row, an open overlay, and the input that
  // keeps focus and its query while rows are appended.
  const [chainActive, setChainActive] = createSignal(0)
  const [chainOpen, setChainOpen] = createSignal(false)
  // Appends still in flight stay visible as disabled rows, and two quick adds
  // are written on top of each other instead of racing the registry snapshot.
  const [chainPending, setChainPending] = createSignal<string[]>([])
  // While the catalog query is active a group collapses in memory only; the
  // persisted fold state is restored untouched when the query is cleared.
  const [searchCollapsed, setSearchCollapsed] = createSignal<Record<string, boolean>>({})
  let chainInput: HTMLInputElement | undefined
  const chainRowRefs = new Map<string, HTMLElement>()
  const [favoriteOverride, setFavoriteOverride] = createSignal<Record<string, boolean>>({})
  const [visibilityOverride, setVisibilityOverride] = createSignal<Record<string, boolean>>({})
  const [saving, setSaving] = createSignal(false)
  const [error, setError] = createSignal<string>()
  // Health countdowns are minute-granular, so a slow tick keeps badges honest.
  const [now, setNow] = createSignal(Date.now())

  const timer = setInterval(() => setNow(Date.now()), 30_000)

  onCleanup(() => clearInterval(timer))

  const [collapse, setCollapse, , collapseReady] = persisted(
    Persist.global("settings.models.providers"),
    ModelProvidersSchema,
    { collapsed: {} },
  )
  const [collapseState] = createResource(
    () => collapseReady.promise ?? Promise.resolve(),
    (promise) => promise.then(() => collapse),
    { initialValue: collapse },
  )

  const snapshot = () => models.query.data
  const state = () => {
    const value = snapshot()

    return value?.available ? value.registry : undefined
  }
  const unavailable = () => {
    const value = snapshot()

    return value && !value.available ? value.message : undefined
  }
  const all = () => state()?.models ?? []
  const visibleAgents = () => (agents.data ?? []).filter((agent: AgentOption) => !agent.hidden)
  const primaryAgents = () => visibleAgents().filter((agent) => agent.mode === "primary")
  const subagents = () => visibleAgents().filter((agent) => agent.mode !== "primary")
  /**
   * Hidden builtins plus the runtime roles that never appear in `/api/agent`.
   * They still resolve a chain, so they belong in the policy editor; treating
   * them as agents anywhere else would create ghost rows.
   */
  const systemRoles = (): AgentOption[] => {
    const hiddenBuiltins = (agents.data ?? []).filter((agent: AgentOption) => agent.hidden)
    const configuredRoles = Object.keys(state()?.chains ?? {})
    const roleNames = [...new Set([...SYSTEM_ROLE_NAMES, ...configuredRoles])]
    const synthesized: AgentOption[] = roleNames
      .filter(
        (name) =>
          !hiddenBuiltins.some((agent) => agent.name === name) && !visibleAgents().some((agent) => agent.name === name),
      )
      .map((name) => ({ id: name, name, mode: "all", hidden: true }))

    return [...hiddenBuiltins, ...synthesized]
  }
  const scopeAgent = (): AgentOption | undefined =>
    [...visibleAgents(), ...systemRoles()].find((agent) => agent.name === scope())
  const activeAgent = () => visibleAgents().find((agent) => agent.name === scope())

  const refOf = (model: RegistryModel) => modelRef(model.providerID, model.id)
  const favorite = (model: RegistryModel) => favoriteOverride()[refOf(model)] ?? model.favorite
  const visible = (model: RegistryModel) => visibilityOverride()[refOf(model)] ?? model.visible

  const favorites = () => all().filter((model) => favorite(model)).map(refOf)
  const hidden = () => all().filter((model) => !visible(model)).map(refOf)

  const searching = () => search().trim().length > 0
  const collapsed = (providerID: string) => collapseState().collapsed[providerID] ?? true
  /**
   * A query paints only the matching groups, expanded, and the fold buttons stay
   * live: while a query is active the fold lives in `searchCollapsed` (default
   * open so matches are visible) so clearing the query restores the persisted
   * per-provider state instead of forcing a global expand.
   */
  const expanded = (providerID: string) =>
    searching() ? !(searchCollapsed()[providerID] ?? false) : !collapsed(providerID)

  const setGroupCollapsed = (providerID: string, value: boolean) => {
    if (searching()) {
      setSearchCollapsed((current) => ({ ...current, [providerID]: value }))

      return
    }

    setCollapse("collapsed", providerID, value)
  }

  const setAllCollapsed = (value: boolean) => {
    if (searching()) {
      setSearchCollapsed(Object.fromEntries(groups().map((group) => [group.providerID, value])))

      return
    }

    for (const group of groups()) setGroupCollapsed(group.providerID, value)
  }

  const formatCost = (value: number) =>
    new Intl.NumberFormat(language.intl(), {
      style: "currency",
      currency: "USD",
      minimumFractionDigits: 2,
      maximumFractionDigits: 2,
    }).format(value)

  const relative = (untilMs: number): string | undefined => {
    const remaining = healthRemaining(untilMs, now())

    if (!remaining) return undefined

    return new Intl.RelativeTimeFormat(language.intl(), { numeric: "auto" }).format(remaining.value, remaining.unit)
  }

  const filtered = createMemo(() => {
    const query = search().trim()

    if (!query) return all()

    return all().filter((model) =>
      matchesModelSearch(query, [model.name, model.id, model.providerID, ...(model.variants ?? [])]),
    )
  })

  const groups = createMemo<Group[]>(() => {
    const byProvider = new Map<string, RegistryModel[]>()

    for (const model of filtered()) byProvider.set(model.providerID, [...(byProvider.get(model.providerID) ?? []), model])

    return [...byProvider.entries()]
      .map(([providerID, items]) => ({
        providerID,
        models: [...items].sort((a, b) => a.name.localeCompare(b.name)),
      }))
      .sort((a, b) => a.providerID.localeCompare(b.providerID))
  })

  const scopeChain = (): string[] => {
    const value = state()

    if (!value) return []
    const agent = scope()

    return agent ? (value.chains[agent] ?? value.chain) : value.chain
  }

  const agentChainDefined = () => {
    const value = state()
    const agent = scope()

    return !!(value && agent && value.chains[agent])
  }

  const focusRef = () => focus() || (all()[0] ? refOf(all()[0]) : "")
  const focusModel = () => all().find((model) => refOf(model) === focusRef())
  const modelAt = (ref: string) => all().find((model) => refOf(model) === ref)
  const nameAt = (ref: string) => modelAt(ref)?.name ?? ref

  const pinnedRef = () => {
    const model = activeAgent()?.model

    return model ? modelRef(model.providerID, model.id) : undefined
  }

  const quotaRule = () => {
    const value = state()

    return value?.quota.models?.[focusRef()] ?? value?.quota.default
  }

  const percentUsed = (model: RegistryModel): number | undefined => {
    const usage = model.usage
    const quota: ModelQuota | undefined = model.quota

    // A zero window means the rollup has no honest denominator.
    if (!usage || usage.windowHours <= 0) return undefined

    if (quota?.maxRequests !== undefined && quota.maxRequests > 0) {
      const used = quota.usedRequests ?? usage.requests

      return Math.max(0, Math.min(100, (used / quota.maxRequests) * 100))
    }

    if (quota?.remainingPercent !== undefined) return Math.max(0, Math.min(100, 100 - quota.remainingPercent))

    if (quota?.maxCostUsd !== undefined && quota.maxCostUsd > 0) {
      const used = quota.usedCostUsd ?? usage.costUsd

      if (used === undefined) return undefined

      return Math.max(0, Math.min(100, (used / quota.maxCostUsd) * 100))
    }

    return undefined
  }

  const remainingPercent = createMemo(() => {
    const values = all().flatMap((model) =>
      model.quota?.remainingPercent !== undefined ? [model.quota.remainingPercent] : [],
    )

    return values.length > 0 ? Math.min(...values) : undefined
  })

  const health = () => state()?.modelHealth ?? []
  // A key leaves rotation for one provider only; it never hides a model.
  const keyHealth = () => state()?.keyHealth ?? []

  async function persist(patch: ModelsConfigPatch, revert?: () => void) {
    setSaving(true)
    setError(undefined)

    try {
      await models.save(patch)
    } catch (cause) {
      revert?.()

      if (cause instanceof ModelsSaveError) {
        const ref = cause.status === 422 ? invalidModelRef(cause.message) : undefined

        setError(
          ref
            ? language.t("settings.models.error.invalidModel", { model: ref })
            : language.t("settings.models.error.save", { message: cause.message }),
        )

        return
      }

      setError(language.t("settings.models.error.save", { message: cause instanceof Error ? cause.message : String(cause) }))
    } finally {
      setSaving(false)
    }
  }

  const toggleFavorite = (model: RegistryModel) => {
    const ref = refOf(model)
    const next = !favorite(model)
    const refs = new Set(favorites())

    if (next) refs.add(ref)
    else refs.delete(ref)
    setFavoriteOverride((current) => ({ ...current, [ref]: next }))
    void persist({ favorites: [...refs] }, () => setFavoriteOverride((current) => clearKey(current, ref)))
  }

  const setVisibility = (model: RegistryModel, next: boolean) => {
    const ref = refOf(model)
    const refs = new Set(hidden())

    if (next) refs.delete(ref)
    else refs.add(ref)
    setVisibilityOverride((current) => ({ ...current, [ref]: next }))
    void persist({ hidden: [...refs] }, () => setVisibilityOverride((current) => clearKey(current, ref)))
  }

  const saveChain = (chain: string[]) => {
    const agent = scope()

    return persist(agent ? { chains: { [agent]: chain } } : { chain })
  }

  const moveChain = (index: number, delta: number) => void saveChain(applyChainMove(scopeChain(), index, delta))

  const dropFromChain = (ref: string) => void saveChain(removeFromChain(scopeChain(), ref))

  const chainFull = () => scopeChain().length >= MAX_CHAIN

  /**
   * Appends a model and keeps the add-search usable: the query, the focus and the
   * open list survive, and the just-added ref paints as a disabled row at once
   * (`chainPending`) instead of waiting for the registry refetch.
   */
  const appendToChain = (ref: string) => {
    const chain = scopeChain()

    // A second Enter on the same row, or one while the chain is full, writes nothing.
    if (!ref || chain.includes(ref) || chainPending().includes(ref) || chain.length >= MAX_CHAIN) return
    setChainPending((current) => [...current, ref])
    setChainActive(0)
    chainInput?.focus({ preventScroll: true })
    // The optimistic row is only honest until the write settles; a failed save
    // drops it and the model returns to the candidate list.
    void saveChain(addToChain(chain, ref)).then(() => {
      if (!scopeChain().includes(ref)) setChainPending((current) => current.filter((item) => item !== ref))
    })
  }

  const addResults = createMemo(() => chainCandidates(all(), scopeChain(), chainQuery()))

  /** Refs written but not yet visible in the registry snapshot. */
  const pendingModels = (): RegistryModel[] => {
    const chain = new Set(scopeChain())
    const byRef = new Map(all().map((model) => [refOf(model), model]))

    return chainPending().flatMap((ref) => {
      const model = byRef.get(ref)

      return model && !chain.has(ref) ? [model] : []
    })
  }

  /** A ref that reached the snapshot no longer needs its optimistic row. */
  createEffect(() => {
    const chain = scopeChain()
    const pending = chainPending()
    const remaining = pending.filter((ref) => !chain.includes(ref))

    if (remaining.length !== pending.length) setChainPending(remaining)
  })

  const chainRows = createMemo(() => {
    const results = addResults()
    const pending = pendingModels()
    const refs = new Set([...chainPending(), ...scopeChain()])

    return searchState({
      candidates: results.candidates.filter((model) => !refs.has(refOf(model))),
      alreadyAdded: [...pending, ...results.alreadyAdded],
      activeIndex: chainActive(),
      limit: CHAIN_SEARCH_LIMIT,
    })
  })

  /** The addable matches and the already-chained matches, before the render cap. */
  const chainCounts = createMemo(() => {
    const rows = chainRows()

    return {
      candidates: rows.enabled + rows.hidden,
      added: rows.rows.filter((row) => row.group === "added").length,
      hidden: rows.hidden,
    }
  })

  const activeOption = () => {
    const rows = chainRows()

    return rows.activeIndex < 0 ? undefined : `settings-models-chain-option-${rows.activeIndex}`
  }

  const runChainCommand = (command: ChainSearchCommand) => {
    const outcome = searchKey(chainRows(), command, scopeChain().length)

    setChainActive(outcome.activeIndex)
    if (outcome.add) appendToChain(outcome.add)

    return outcome
  }

  // Keep the active row inside the list's own scroll box; the page never moves.
  createEffect(() => {
    const rows = chainRows()

    if (!chainOpen() || rows.activeIndex < 0) return
    chainRowRefs.get(`settings-models-chain-option-${rows.activeIndex}`)?.scrollIntoView({ block: "nearest" })
  })

  // A new query starts from the top of its own results.
  createEffect(() => {
    chainQuery()
    setChainActive(0)
  })

  /**
   * `models.notes[ref]` is the single editable note, published on the model
   * row itself. `models.agentNotes` is still readable for a legacy badge; it is
   * never written from this tab.
   */
  const noteOf = (model: RegistryModel) => model.note
  const noteAt = (ref: string) => modelAt(ref)?.note
  const legacyNote = (ref: string): string | undefined => {
    const role = scope()

    return role ? state()?.agentNotes[role]?.[ref] : undefined
  }

  const saveQuota = (key: "maxCostUsd" | "maxRequests", raw: string) => {
    const value = parseNumber(raw)

    // Maps merge key-by-key, so an omitted key cannot clear a stored budget.
    if (value === undefined) return
    void persist({ quota: { models: { [focusRef()]: { [key]: value } } } })
  }

  /** Writes one note for one model ref; an empty value clears it. */
  const saveNoteFor = (ref: string, raw: string) => {
    if (!ref) return
    const notes: Record<string, string> = {}

    // The map is replaced wholesale, so every other note must travel with the
    // write or it would be dropped.
    for (const model of all()) if (model.note) notes[refOf(model)] = model.note
    if (raw) notes[ref] = raw
    else delete notes[ref]
    void persist({ notes })
  }

  const saveNote = (raw: string) => saveNoteFor(focusRef(), raw)

  const focusNote = () => {
    const ref = focusRef()

    return ref ? (noteAt(ref) ?? "") : ""
  }

  const canRetry = () => snapshot() !== undefined && unavailable() !== undefined

  createEffect(() => {
    if (!props.active || !props.provider) return
    const provider = props.provider

    setGroupCollapsed(provider, false)
    queueMicrotask(() => props.onReveal?.())
  })

  // A scope that disappears from the role list falls back to Global.
  createEffect(() => {
    const names = [...visibleAgents(), ...systemRoles()].map((agent) => agent.name)
    const current = scope()

    if (current && !names.includes(current)) setScope("")
  })

  function ModelRow(rowProps: { model: RegistryModel }) {
    const model = rowProps.model
    const note = () => noteOf(model)
    const percent = () => percentUsed(model)
    const progress = () => {
      const value = percent()

      return value === undefined ? undefined : { percent: Math.round(value) }
    }
    const price = () => model.cost?.[0]
    const focused = () => focus() === refOf(model)

    return (
      <div
        data-component="settings-models-row"
        data-focused={focused() ? "" : undefined}
        class="flex flex-wrap items-center gap-x-3 gap-y-2 border-t border-v2-border-border-muted py-2 first:border-t-0"
        onClick={(event) => {
          const target = event.target

          // The row focuses the model for notes; its own controls must win.
          if (target instanceof Element && target.closest("[data-row-control]")) return
          setFocus(refOf(model))
        }}
      >
        <div class="flex min-w-0 flex-1 flex-col gap-0.5">
          <div class="flex min-w-0 items-center gap-1.5">
            <span class="min-w-0 truncate text-12-regular text-v2-text-text-base">{model.name}</span>
            <Show when={note()}>
              {(value) => (
                <span
                  class="flex min-w-0 shrink-0 items-center gap-1 text-v2-icon-icon-muted"
                  title={value()}
                  aria-label={language.t("settings.models.note.indicator", { note: value() })}
                >
                  <Icon name="outline-text" size="small" />
                  <bdi class="max-w-40 truncate text-12-regular text-v2-text-text-muted">{value()}</bdi>
                </span>
              )}
            </Show>
            <Show when={model.unavailable}>
              {(value) => (
                <span
                  data-component="settings-models-unavailable-badge"
                  class="shrink-0 rounded-full border border-v2-state-border-warning bg-v2-state-bg-warning px-1.5 text-12-regular text-v2-state-fg-warning"
                  title={value().message ?? value().kind}
                >
                  <Show
                    when={relative(value().untilMs)}
                    fallback={language.t("settings.models.unavailable.badgeNow", { kind: value().kind })}
                  >
                    {(until) => language.t("settings.models.unavailable.badge", { kind: value().kind, until: until() })}
                  </Show>
                </span>
              )}
            </Show>
            <For each={model.variants ?? []}>
              {(variant) => <span class="shrink-0 text-12-regular text-v2-text-text-faint">{variant}</span>}
            </For>
          </div>
          <div class="flex flex-wrap items-center gap-2 text-12-regular text-v2-text-text-muted">
            <span class="whitespace-nowrap" aria-label={language.t("settings.models.cost.title")}>
              <Show when={price()} fallback={<span aria-hidden="true">—</span>}>
                {(cost) => (
                  <>
                    <span class="sr-only">{language.t("settings.models.cost.title")}: </span>
                    {formatCost(cost().input)}
                    <span aria-hidden="true"> / </span>
                    {formatCost(cost().output)}
                  </>
                )}
              </Show>
            </span>
            <Show when={model.usage}>{(usage) => <span>{language.t("settings.models.usage.requests", { requests: usage().requests })}</span>}</Show>
          </div>
        </div>
        <div class="flex shrink-0 items-center gap-1.5" data-row-control>
          <Show
            when={progress()}
            fallback={
              <span class="w-24 text-end text-12-regular text-v2-text-text-faint">
                {language.t("settings.models.usage.unknown")}
              </span>
            }
          >
            {(value) => (
              <div
                class="h-1.5 w-24 overflow-hidden rounded-full bg-v2-background-bg-layer-02"
                role="progressbar"
                aria-label={language.t("settings.models.usage.bar", { model: model.name })}
                aria-valuemin={0}
                aria-valuemax={100}
                aria-valuenow={value().percent}
                aria-valuetext={`${value().percent}%`}
              >
                <div class="h-full rounded-full bg-v2-text-text-accent" style={{ width: `${value().percent}%` }} />
              </div>
            )}
          </Show>
          <button
            type="button"
            class="flex size-6 items-center justify-center rounded text-12-regular text-v2-text-text-muted hover:text-v2-text-text-base focus-visible:outline focus-visible:outline-2 focus-visible:outline-v2-border-border-focus disabled:opacity-50"
            aria-pressed={favorite(model)}
            aria-label={language.t("settings.models.favorite.toggle", { model: model.name })}
            disabled={saving()}
            onClick={() => toggleFavorite(model)}
          >
            <span aria-hidden="true">{favorite(model) ? "★" : "☆"}</span>
          </button>
          <Switch checked={visible(model)} onChange={(next) => setVisibility(model, next)} hideLabel>
            {language.t("settings.models.visible.toggle", { model: model.name })}
          </Switch>
        </div>
      </div>
    )
  }

  return (
    <>
      <div class="settings-tab-header settings-tab-header--stacked">
        <div class="settings-tab-header-row">
          <div class="flex min-w-0 flex-col gap-1">
            <h2 class="settings-tab-title">{language.t("settings.models.title")}</h2>
            <span class="text-12-regular text-v2-text-text-muted">{language.t("settings.models.description")}</span>
          </div>
          <div class="settings-tab-header-actions">{props.headerAction}</div>
        </div>
        <div class="flex flex-col gap-1">
          <span class="text-12-medium text-v2-text-text-muted">{language.t("settings.models.catalog.scopeLabel")}</span>
          <SettingsSearchField
            value={search()}
            active={props.active ?? true}
            onInput={setSearch}
            placeholder={language.t("dialog.model.search.placeholder")}
          />
        </div>
      </div>

      <div class="settings-tab-body settings-models">
        <Show when={error()}>
          {(message) => (
            <div
              role="alert"
              data-testid="settings-models-error"
              class="flex items-start justify-between gap-3 rounded-md border border-v2-state-border-danger bg-v2-state-bg-danger px-3 py-2 text-12-regular text-v2-state-fg-danger"
            >
              <span>{message()}</span>
              <IconButton
                size="small"
                variant="ghost-muted"
                icon={<Icon name="xmark-small" size="small" />}
                aria-label={language.t("common.dismiss")}
                onClick={() => setError(undefined)}
              />
            </div>
          )}
        </Show>

        <Show
          when={snapshot() !== undefined}
          fallback={
            <div class="settings-models-status">
              {language.t("common.loading")}
              {language.t("common.loading.ellipsis")}
            </div>
          }
        >
          <Show
            when={state()}
            fallback={
              <div
                role="status"
                data-testid="settings-models-unavailable"
                class="flex flex-col items-start gap-2 rounded-md border border-v2-border-border-base bg-v2-background-bg-layer-01 px-3 py-2"
              >
                <span class="text-12-medium text-v2-text-text-base">
                  {language.t("settings.models.unavailable.title")}
                </span>
                <span class="text-12-regular text-v2-text-text-muted">
                  {language.t("settings.models.unavailable.description")}
                </span>
                <Show when={unavailable()}>
                  {(message) => <span class="text-12-regular text-v2-text-text-faint">{message()}</span>}
                </Show>
                <Show when={canRetry()}>
                  <Button size="small" variant="neutral" onClick={() => void models.query.refetch()}>
                    {language.t("usage.retry")}
                  </Button>
                </Show>
              </div>
            }
          >
            <div class="flex flex-col gap-6 md:grid md:grid-cols-[minmax(0,1fr)_minmax(320px,380px)] md:items-start">
              <section class="flex min-w-0 flex-col gap-3" aria-label={language.t("settings.models.column.catalog")}>
                <div class="flex items-center justify-between gap-2">
                  <h3 class="text-12-medium text-v2-text-text-base">{language.t("settings.models.column.catalog")}</h3>
                  <div class="flex items-center gap-1">
                    <Button size="small" variant="neutral" disabled={groups().length === 0} onClick={() => setAllCollapsed(false)}>
                      {language.t("settings.models.catalog.expandAll")}
                    </Button>
                    <Button size="small" variant="neutral" disabled={groups().length === 0} onClick={() => setAllCollapsed(true)}>
                      {language.t("settings.models.catalog.collapseAll")}
                    </Button>
                  </div>
                </div>
                <Show
                  when={filtered().length > 0}
                  fallback={
                    <Show
                      when={searching()}
                      fallback={<div class="settings-models-status">{language.t("dialog.model.empty")}</div>}
                    >
                      <div class="settings-tab-search-empty">
                        <SettingsSearchEmpty query={search()} />
                      </div>
                    </Show>
                  }
                >
                  <For each={groups()}>
                    {(group) => (
                      <section class="flex min-w-0 flex-col" data-component="settings-models-provider">
                        <h3 class="settings-models-group-header">
                          <button
                            type="button"
                            class="settings-models-group-trigger"
                            aria-expanded={expanded(group.providerID)}
                            onClick={() => setGroupCollapsed(group.providerID, expanded(group.providerID))}
                          >
                            <span class="settings-models-group-chevron">
                              <Icon
                                name="chevron-down"
                                size="small"
                                classList={{ collapsed: !expanded(group.providerID) }}
                              />
                            </span>
                            <span class="settings-models-group-label">
                              <ProviderIcon id={group.providerID} class="size-4 shrink-0" />
                              <bdi class="settings-models-group-title">{group.providerID}</bdi>
                              <span class="text-12-regular text-v2-text-text-faint">
                                {language.t("settings.models.catalog.count", { count: group.models.length })}
                              </span>
                            </span>
                          </button>
                        </h3>
                        <Show when={expanded(group.providerID)}>
                          <div class="flex flex-col">
                            <For each={group.models}>{(model) => <ModelRow model={model} />}</For>
                          </div>
                        </Show>
                      </section>
                    )}
                  </For>
                </Show>
              </section>

              <section class="flex min-w-0 flex-col gap-5" aria-label={language.t("settings.models.column.control")}>
                <div class="flex flex-col gap-1.5">
                  <label class="text-12-regular text-v2-text-text-muted" for="settings-models-scope">
                    {language.t("settings.models.scope.label")}
                  </label>
                  <select
                    id="settings-models-scope"
                    class="h-7 w-full rounded border border-v2-border-border-base bg-v2-background-bg-base px-2 text-12-regular text-v2-text-text-base"
                    value={scope()}
                    onChange={(event) => setScope(event.currentTarget.value)}
                  >
                    <option value="">{language.t("settings.models.scope.global")}</option>
                    <Show when={primaryAgents().length > 0}>
                      <optgroup label={language.t("settings.models.scope.group.primary")}>
                        <For each={primaryAgents()}>
                          {(agent) => <option value={agent.name}>{agent.name}</option>}
                        </For>
                      </optgroup>
                    </Show>
                    <Show when={subagents().length > 0}>
                      <optgroup label={language.t("settings.models.scope.group.subagent")}>
                        <For each={subagents()}>
                          {(agent) => <option value={agent.name}>{agent.name}</option>}
                        </For>
                      </optgroup>
                    </Show>
                    <Show when={systemRoles().length > 0}>
                      <optgroup label={language.t("settings.models.scope.group.system")}>
                        <For each={systemRoles()}>
                          {(role) => <option value={role.name}>{role.name}</option>}
                        </For>
                      </optgroup>
                    </Show>
                  </select>
                  <Show when={activeAgent()}>
                    {(agent) => (
                      <span class="text-12-regular text-v2-text-text-faint">
                        {language.t("settings.models.agent.mode", { mode: agent().mode })}
                      </span>
                    )}
                  </Show>
                  <Show when={scopeAgent()?.hidden}>
                    <span class="text-12-regular text-v2-text-text-faint">
                      {language.t("settings.models.scope.systemNote")}
                    </span>
                  </Show>
                  <Show when={scope() && !agentChainDefined()}>
                    <span
                      data-testid="settings-models-inherits-global"
                      class="text-12-regular text-v2-text-text-faint"
                    >
                      {language.t("settings.models.scope.inherits", { role: scope() })}
                    </span>
                  </Show>
                  <Show when={pinnedRef()}>
                    {(ref) => (
                      <div
                        class="flex flex-col gap-0.5 rounded border border-v2-border-border-base px-2 py-1"
                        data-component="settings-models-pinned"
                      >
                        <span class="text-12-regular text-v2-text-text-muted">
                          {language.t("settings.models.agent.pinned")}
                        </span>
                        <bdi class="truncate text-12-regular text-v2-text-text-base">{ref()}</bdi>
                        <span class="text-12-regular text-v2-text-text-faint">
                          {language.t("settings.models.agent.pinnedHint")}
                        </span>
                      </div>
                    )}
                  </Show>
                </div>

                <div class="flex flex-col gap-1.5">
                  <h3 class="text-12-medium text-v2-text-text-base">{language.t("settings.models.chain.title")}</h3>
                  <span class="text-12-regular text-v2-text-text-muted">
                    {language.t("settings.models.chain.description")}
                  </span>
                  <Show when={scope()}>
                    {(agent) => (
                      <span class="text-12-regular text-v2-text-text-muted">
                        {language.t("settings.models.chain.forAgent", { agent: agent() })}
                      </span>
                    )}
                  </Show>
                  <span class="text-12-regular text-v2-text-text-faint">
                    {language.t("settings.models.chain.count", { count: scopeChain().length, max: MAX_CHAIN })}
                    {" · "}
                    {language.t("settings.models.chain.hops", { hops: state()?.fallback.maxHops ?? 0 })}
                  </span>
                  <Show
                    when={scopeChain().length > 0}
                    fallback={
                      <span class="text-12-regular text-v2-text-text-muted">
                        {scope() && !agentChainDefined()
                          ? language.t("settings.models.chain.emptyAgent")
                          : language.t("settings.models.chain.empty")}
                      </span>
                    }
                  >
                    <ol class="flex flex-col gap-1">
                      <For each={scopeChain()}>
                        {(ref, index) => (
                          <li class="flex flex-col gap-1 rounded border border-v2-border-border-base px-2 py-1">
                            <div class="flex items-center gap-1">
                              <span class="shrink-0 text-12-regular text-v2-text-text-faint">{index() + 1}.</span>
                              <span class="flex min-w-0 flex-1 flex-col">
                                <bdi class="truncate text-12-regular text-v2-text-text-base">{nameAt(ref)}</bdi>
                                <bdi class="truncate text-12-regular text-v2-text-text-faint">{ref}</bdi>
                                <Show when={legacyNote(ref)}>
                                  {(legacy) => (
                                    <span
                                      data-testid="settings-models-legacy-note"
                                      class="truncate text-12-regular text-v2-text-text-faint"
                                      title={legacy()}
                                    >
                                      {language.t("settings.models.notes.legacy", { agent: scope() })}
                                    </span>
                                  )}
                                </Show>
                              </span>
                              <Show when={noteAt(ref)}>
                                {(note) => (
                                  <span
                                    class="flex shrink-0 text-v2-icon-icon-muted"
                                    title={note()}
                                    aria-label={language.t("settings.models.note.indicator", { note: note() })}
                                  >
                                    <Icon name="outline-text" size="small" />
                                  </span>
                                )}
                              </Show>
                              <IconButton
                                size="small"
                                variant="ghost-muted"
                                icon={<Icon name="outline-arrow-left" size="small" />}
                                aria-label={language.t("settings.models.chain.moveUp", { model: ref })}
                                disabled={saving() || index() === 0}
                                onClick={() => moveChain(index(), -1)}
                              />
                              <IconButton
                                size="small"
                                variant="ghost-muted"
                                icon={<Icon name="outline-arrow-right" size="small" />}
                                aria-label={language.t("settings.models.chain.moveDown", { model: ref })}
                                disabled={saving() || index() === scopeChain().length - 1}
                                onClick={() => moveChain(index(), 1)}
                              />
                              <IconButton
                                size="small"
                                variant="ghost-muted"
                                icon={<Icon name="outline-trash" size="small" />}
                                aria-label={language.t("settings.models.chain.remove", { model: ref })}
                                disabled={saving()}
                                onClick={() => dropFromChain(ref)}
                              />
                            </div>
                            {/* The row edits the one note for this ref; blur or Cmd/Ctrl+Enter commits it. */}
                            <Show keyed when={`${ref}|${noteAt(ref) ?? ""}`}>
                              {(_key) => (
                                <NoteTextarea
                                  value={noteAt(ref) ?? ""}
                                  label={language.t("settings.models.chain.note", { model: nameAt(ref) })}
                                  placeholder={language.t("settings.models.chain.notePlaceholder")}
                                  disabled={saving()}
                                  onCommit={(value) => saveNoteFor(ref, value)}
                                />
                              )}
                            </Show>
                          </li>
                        )}
                      </For>
                    </ol>
                  </Show>
                  {/* Two search boxes share the tab; each one names its own scope. */}
                  <span class="text-12-medium text-v2-text-text-muted" id="settings-models-chain-label">
                    {language.t("settings.models.chain.addScope")}
                  </span>
                  <div class="relative">
                    <TextInput
                      ref={(element) => {
                        chainInput = element
                      }}
                      type="search"
                      class="w-full"
                      value={chainQuery()}
                      placeholder={language.t("settings.models.chain.addPlaceholder")}
                      aria-label={language.t("settings.models.chain.addPlaceholder")}
                      role="combobox"
                      aria-expanded={chainOpen() && chainRows().rows.length > 0}
                      aria-controls="settings-models-chain-listbox"
                      aria-autocomplete="list"
                      aria-activedescendant={chainOpen() ? activeOption() : undefined}
                      showClearButton={!!chainQuery()}
                      clearIcon="circle-xmark"
                      clearLabel={language.t("settings.models.chain.addClear")}
                      onClearClick={() => {
                        setChainQuery("")
                        chainInput?.focus({ preventScroll: true })
                      }}
                      spellcheck={false}
                      autocorrect="off"
                      autocomplete="off"
                      autocapitalize="off"
                      onInput={(event) => {
                        setChainQuery(event.currentTarget.value)
                        setChainOpen(true)
                      }}
                      onFocus={() => setChainOpen(true)}
                      onBlur={() => setChainOpen(false)}
                      onKeyDown={(event) => {
                        if (event.isComposing) return

                        const key = event.key

                        if (key === "ArrowDown" || key === "ArrowUp") {
                          event.preventDefault()
                          setChainOpen(true)
                          runChainCommand(key === "ArrowDown" ? "next" : "previous")

                          return
                        }

                        if (key === "Home" || key === "End") {
                          event.preventDefault()
                          setChainOpen(true)
                          runChainCommand(key === "Home" ? "first" : "last")

                          return
                        }

                        if (key === "Enter") {
                          // A refused Enter (already added, or a full chain) is inert, never a save.
                          event.preventDefault()
                          runChainCommand("enter")

                          return
                        }

                        if (key === "Escape") {
                          event.preventDefault()
                          // Escape empties the query first, then closes the list.
                          if (chainQuery()) setChainQuery("")
                          else setChainOpen(false)

                          return
                        }

                        if (key === "Tab") setChainOpen(false)
                      }}
                    />
                    <div class="flex flex-wrap items-baseline justify-between gap-2 pt-1">
                      <span
                        data-testid="settings-models-chain-count"
                        class="text-12-regular text-v2-text-text-muted"
                      >
                        {language.t("settings.models.chain.addResults", {
                          count: chainCounts().candidates,
                        })}
                        {chainCounts().added > 0
                          ? ` · ${language.t("settings.models.chain.addResultsAdded", { count: chainCounts().added })}`
                          : ""}
                      </span>
                      <span
                        data-testid="settings-models-chain-cap"
                        data-full={chainFull() ? "" : undefined}
                        classList={{
                          "text-12-regular": true,
                          "text-v2-state-fg-warning": chainFull(),
                          "text-v2-text-text-faint": !chainFull(),
                        }}
                      >
                        {chainFull()
                          ? language.t("settings.models.chain.full", { max: MAX_CHAIN })
                          : language.t("settings.models.chain.cap", {
                              count: scopeChain().length,
                              max: MAX_CHAIN,
                            })}
                      </span>
                    </div>
                    {/*
                      The list is an overlay: it never pushes the panel below it, and
                      it scrolls itself so the page does not move on arrow keys.
                    */}
                    <Show when={chainOpen()}>
                      <div class="absolute inset-x-0 top-full z-20 mt-1 max-h-64 overflow-y-auto rounded border border-v2-border-border-base bg-v2-background-bg-layer-01 py-1 shadow-lg">
                        <div
                          id="settings-models-chain-listbox"
                          role="listbox"
                          aria-label={language.t("settings.models.chain.addPlaceholder")}
                          aria-describedby="settings-models-chain-label"
                        >
                          <Show
                            when={chainRows().rows.length > 0}
                            fallback={
                              <span class="block px-2 py-1 text-12-regular text-v2-text-text-muted">
                                {chainQuery().trim()
                                  ? language.t("settings.models.chain.addEmpty", { query: chainQuery().trim() })
                                  : language.t("settings.models.chain.addAllAdded")}
                              </span>
                            }
                          >
                            <For each={chainRows().rows}>
                              {(row, index) => {
                                const id = () => `settings-models-chain-option-${index()}`
                                const active = () => chainRows().activeIndex === index()
                                const disabled = () =>
                                  row.disabled || chainFull() || (saving() && row.group === "candidate")
                                const title = `${row.model.name} — ${row.ref}`

                                return (
                                  <>
                                    <Show
                                      when={row.group === "added" && index() === chainRows().enabled}
                                    >
                                      {/* Already-chained matches move below a labelled separator. */}
                                      <div
                                        data-component="settings-models-chain-separator"
                                        role="presentation"
                                        class="mt-1 flex items-center gap-2 border-t border-v2-border-border-muted px-2 pt-1.5 pb-1 text-12-regular text-v2-text-text-faint"
                                      >
                                        {language.t("settings.models.chain.addSeparator", {
                                          count: chainCounts().added,
                                        })}
                                      </div>
                                    </Show>
                                    <div
                                      data-component="settings-models-chain-option"
                                      data-group={row.group}
                                      data-active={active() ? "" : undefined}
                                      ref={(element) => {
                                        chainRowRefs.set(id(), element)
                                        onCleanup(() => chainRowRefs.delete(id()))
                                      }}
                                    >
                                      <button
                                        type="button"
                                        id={id()}
                                        role="option"
                                        tabIndex={-1}
                                        aria-selected={active()}
                                        aria-disabled={disabled()}
                                        data-active={active() ? "" : undefined}
                                        classList={{
                                          "flex w-full min-w-0 flex-col items-start gap-0 rounded px-2 py-1 text-start":
                                            true,
                                          "text-12-regular": true,
                                          "text-v2-text-text-base": !row.disabled,
                                          "text-v2-text-text-faint": row.disabled,
                                          "bg-v2-overlay-simple-overlay-hover": active(),
                                          "hover:bg-v2-overlay-simple-overlay-hover": !disabled(),
                                          "focus-visible:outline focus-visible:outline-2 focus-visible:outline-v2-border-border-focus":
                                            true,
                                          "opacity-60": row.disabled && !chainFull(),
                                          "cursor-not-allowed": disabled(),
                                        }}
                                        title={title}
                                        onMouseDown={(event) => {
                                          // Keep the input's focus (and the open list) on click.
                                          event.preventDefault()
                                        }}
                                        onMouseEnter={() => setChainActive(index())}
                                        onClick={() => {
                                          if (!disabled()) appendToChain(row.ref)
                                        }}
                                      >
                                        <bdi class="w-full min-w-0 truncate">
                                          <Highlighted text={row.model.name} query={chainQuery()} />
                                        </bdi>
                                        <bdi class="w-full min-w-0 truncate text-12-regular text-v2-text-text-faint">
                                          <Highlighted text={row.ref} query={chainQuery()} />
                                        </bdi>
                                      </button>
                                      <Show when={disabled() && (chainFull() || row.disabled)}>
                                        <span class="block px-2 pb-1 text-12-regular text-v2-text-text-faint">
                                          {chainFull()
                                            ? language.t("settings.models.chain.full", { max: MAX_CHAIN })
                                            : language.t("settings.models.chain.alreadyAdded")}
                                        </span>
                                      </Show>
                                    </div>
                                  </>
                                )
                              }}
                            </For>
                          </Show>
                        </div>
                        <Show when={chainRows().hidden > 0}>
                          <div
                            data-testid="settings-models-chain-hint"
                            class="border-t border-v2-border-border-muted px-2 pt-1.5 pb-1 text-12-regular text-v2-text-text-muted"
                          >
                            {language.t("settings.models.chain.addMore", {
                              shown: chainRows().enabled,
                              total: chainCounts().candidates,
                            })}
                          </div>
                        </Show>
                      </div>
                    </Show>
                  </div>
                </div>

                <div class="flex flex-col gap-1.5">
                  <h3 class="text-12-medium text-v2-text-text-base">{language.t("settings.models.notes.title")}</h3>
                  <span class="text-12-regular text-v2-text-text-faint">
                    <bdi>{focusRef() || language.t("settings.models.notes.noModel")}</bdi>
                  </span>
                  <Show
                    when={focusModel()}
                    fallback={
                      <span class="text-12-regular text-v2-text-text-muted">
                        {language.t("settings.models.notes.noModel")}
                      </span>
                    }
                  >
                    {/*
                      One note per model ref. The chain rows and the catalog edit
                      the same `models.notes[ref]`; a legacy `agentNotes` entry
                      only earns a badge and is never written again.
                    */}
                    <Show when={legacyNote(focusRef())}>
                      {(legacy) => (
                        <span
                          data-testid="settings-models-legacy-note"
                          class="text-12-regular text-v2-text-text-faint"
                          title={legacy()}
                        >
                          {language.t("settings.models.notes.legacy", { agent: scope() })}
                        </span>
                      )}
                    </Show>
                    <Show keyed when={`${focusRef()}|`}>
                      {(_key) => (
                        <NoteTextarea
                          value={focusNote()}
                          label={language.t("settings.models.notes.title")}
                          placeholder={language.t("settings.models.notes.placeholder")}
                          disabled={saving()}
                          onCommit={saveNote}
                        />
                      )}
                    </Show>
                    <span class="text-12-regular text-v2-text-text-faint">
                      {language.t("settings.models.notes.hint")}
                    </span>
                  </Show>
                </div>

                <div class="flex flex-col gap-1.5">
                  <h3 class="text-12-medium text-v2-text-text-base">{language.t("settings.models.health.title")}</h3>
                  <span class="text-12-regular text-v2-text-text-muted">
                    {language.t("settings.models.health.description")}
                  </span>
                  <Show
                    when={health().length > 0 || keyHealth().length > 0}
                    fallback={
                      <span class="text-12-regular text-v2-text-text-muted">
                        {language.t("settings.models.health.empty")}
                      </span>
                    }
                  >
                    <ul class="flex flex-col gap-1">
                      <For each={health()}>
                        {(entry) => (
                          <li class="flex items-center justify-between gap-2 rounded border border-v2-border-border-base px-2 py-1">
                            <bdi class="min-w-0 truncate text-12-regular text-v2-text-text-base">
                              {entry.reference}
                            </bdi>
                            <span class="shrink-0 text-12-regular text-v2-text-text-muted" title={entry.message}>
                              <Show
                                when={relative(entry.untilMs)}
                                fallback={language.t("settings.models.unavailable.badgeNow", { kind: entry.kind })}
                              >
                                {(until) =>
                                  language.t("settings.models.health.entry", { kind: entry.kind, until: until() })
                                }
                              </Show>
                            </span>
                          </li>
                        )}
                      </For>
                      {/*
                        The credential axis: a key here only leaves rotation for its
                        own provider on the same model; no model is retired by it.
                      */}
                      <For each={keyHealth()}>
                        {(entry) => (
                          <li
                            data-testid="settings-models-key-health"
                            data-kind={entry.kind}
                            class="flex items-center justify-between gap-2 rounded border border-v2-border-border-base px-2 py-1"
                          >
                            <bdi class="min-w-0 truncate text-12-regular text-v2-text-text-base">
                              {entry.providerID} ·{" "}
                              {entry.credentialID ?? language.t("settings.models.health.envKey")}
                            </bdi>
                            <span class="shrink-0 text-12-regular text-v2-text-text-muted">
                              <Show
                                when={relative(entry.untilMs)}
                                fallback={language.t("settings.models.health.keyNow", { kind: entry.kind })}
                              >
                                {(until) =>
                                  language.t("settings.models.health.key", { kind: entry.kind, until: until() })
                                }
                              </Show>
                            </span>
                          </li>
                        )}
                      </For>
                    </ul>
                  </Show>
                </div>

                <Collapsible
                  variant="ghost"
                  data-component="settings-models-limits"
                  // Opening the usage section is what flips the registry to the
                  // usage body; the sparse paint stays visible until it lands.
                  onOpenChange={(open: boolean) => setUsageOpen(open)}
                >
                  <Collapsible.Trigger>
                    <span>{language.t("settings.models.limits.title")}</span>
                    <Show when={usageOpen() && models.query.isFetching}>
                      <span data-testid="settings-models-usage-refreshing" class="text-12-regular text-v2-text-text-faint">
                        {language.t("settings.models.usage.refreshing")}
                      </span>
                    </Show>
                    <Collapsible.Arrow />
                  </Collapsible.Trigger>
                  <Collapsible.Content>
                    <div class="flex flex-col gap-3 pt-2">
                      <div class="flex flex-col gap-1.5">
                        <h3 class="text-12-medium text-v2-text-text-base">{language.t("settings.models.quota.title")}</h3>
                        <span class="text-12-regular text-v2-text-text-muted">
                          {language.t("settings.models.quota.description")}
                        </span>
                        <label class="text-12-regular text-v2-text-text-muted" for="settings-models-focus">
                          {language.t("settings.models.focus.label")}
                        </label>
                        {/* TODO: this native select still lists every model with no search; it needs the same combobox as the chain add-search. */}
                        <select
                          id="settings-models-focus"
                          class="h-7 w-full rounded border border-v2-border-border-base bg-v2-background-bg-base px-2 text-12-regular text-v2-text-text-base"
                          value={focusRef()}
                          onChange={(event) => setFocus(event.currentTarget.value)}
                        >
                          <For each={all()}>{(model) => <option value={refOf(model)}>{model.name}</option>}</For>
                        </select>
                        <label class="text-12-regular text-v2-text-text-muted" for="settings-models-quota-cost">
                          {language.t("settings.models.quota.maxCost")}
                        </label>
                        <CommitInput
                          numeric
                          value={quotaRule()?.maxCostUsd !== undefined ? String(quotaRule()!.maxCostUsd) : ""}
                          label={language.t("settings.models.quota.maxCost")}
                          placeholder={language.t("settings.models.quota.unset")}
                          disabled={saving()}
                          onCommit={(value) => saveQuota("maxCostUsd", value)}
                        />
                        <label class="text-12-regular text-v2-text-text-muted" for="settings-models-quota-requests">
                          {language.t("settings.models.quota.maxRequests")}
                        </label>
                        <CommitInput
                          numeric
                          value={quotaRule()?.maxRequests !== undefined ? String(quotaRule()!.maxRequests) : ""}
                          label={language.t("settings.models.quota.maxRequests")}
                          placeholder={language.t("settings.models.quota.unset")}
                          disabled={saving()}
                          onCommit={(value) => saveQuota("maxRequests", value)}
                        />
                      </div>

                      <div class="flex flex-col gap-1.5 rounded-md border border-v2-border-border-base px-3 py-2">
                        <h3 class="text-12-medium text-v2-text-text-base">{language.t("settings.models.summary.title")}</h3>
                        <div class="flex flex-wrap items-baseline justify-between gap-2 text-12-regular text-v2-text-text-muted">
                          <span>{language.t("settings.models.summary.window")}</span>
                          <span class="text-v2-text-text-base">
                            {language.t("settings.models.summary.hours", { hours: state()?.usage.windowHours ?? 0 })}
                          </span>
                        </div>
                        <div class="flex flex-wrap items-baseline justify-between gap-2 text-12-regular text-v2-text-text-muted">
                          <span>{language.t("settings.models.summary.requests")}</span>
                          <span class="text-v2-text-text-base">
                            {new Intl.NumberFormat(language.intl()).format(state()?.usage.totalRequests ?? 0)}
                          </span>
                        </div>
                        <div class="flex flex-wrap items-baseline justify-between gap-2 text-12-regular text-v2-text-text-muted">
                          <span>{language.t("settings.models.summary.cost")}</span>
                          <span class="text-v2-text-text-base">{formatCost(state()?.usage.totalCostUsd ?? 0)}</span>
                        </div>
                        <div class="flex flex-wrap items-baseline justify-between gap-2 text-12-regular text-v2-text-text-muted">
                          <span>{language.t("settings.models.summary.remaining")}</span>
                          <span class="text-v2-text-text-base">
                            <Show when={remainingPercent()} fallback={language.t("settings.models.usage.unknown")}>
                              {(value) => `${Math.max(0, Math.min(100, Math.round(value())))}%`}
                            </Show>
                          </span>
                        </div>
                        <Show when={saving()}>
                          <span class="text-12-regular text-v2-text-text-faint">{language.t("common.saving")}</span>
                        </Show>
                      </div>
                    </div>
                  </Collapsible.Content>
                </Collapsible>
              </section>
            </div>
          </Show>
        </Show>
      </div>
    </>
  )
}
