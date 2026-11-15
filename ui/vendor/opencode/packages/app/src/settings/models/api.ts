import { useQuery, useQueryClient } from "@tanstack/solid-query"
import { useServerSDK } from "@/runtime/server/client"
import { serverMessage, useAgenticRequest } from "@/settings/agentic/config"

/**
 * Typed client for `GET /api/models/registry` and `PUT /api/config/models`.
 *
 * The shape is the frozen contract from `docs/17-interfaces.md` (decisions-wire):
 * optional fields are omitted, never null; model references are `provider/model`.
 * Responses are parsed defensively because the route lands in a parallel wave:
 * a `404` becomes an explicit "server update pending" snapshot, never placeholders.
 */

export interface ModelCost {
  input: number
  output: number
  cache?: { read?: number; write?: number }
}

export interface ModelLimit {
  context?: number
  input?: number
  output?: number
}

export interface ModelUsage {
  requests: number
  tokensInput?: number
  tokensOutput?: number
  tokensReasoning?: number
  costUsd?: number
  windowHours: number
}

export interface ModelQuota {
  maxCostUsd?: number
  maxRequests?: number
  usedCostUsd?: number
  usedRequests?: number
  remainingPercent?: number
  limited?: boolean
}

/** A temporary, self-clearing exclusion published by the registry. */
export interface ModelUnavailable {
  untilMs: number
  kind: string
  message?: string
}

/** One entry of `registry.modelHealth`, flattened for the health list. */
export interface ModelHealth {
  providerID: string
  id: string
  reference: string
  untilMs: number
  kind: string
  message?: string
}

/**
 * One entry of `registry.keyHealth`: the credential axis of health.
 *
 * A model in `modelHealth` is retired for everyone; a key here only leaves
 * rotation for its own provider, on the same model. `credentialID` is omitted
 * for a provider's environment key, which owns no stored credential row.
 */
export interface KeyHealth {
  providerID: string
  credentialID?: string
  untilMs: number
  kind: string
  failures: number
}

export interface RegistryModel {
  providerID: string
  id: string
  name: string
  variants?: string[]
  cost?: ModelCost[]
  limit?: ModelLimit
  visible: boolean
  favorite: boolean
  note?: string
  usage?: ModelUsage
  quota?: ModelQuota
  unavailable?: ModelUnavailable
}

export interface FallbackPolicy {
  on: string[]
  maxHops: number
}

export interface QuotaRule {
  maxCostUsd?: number
  maxRequests?: number
}

export interface QuotaConfig {
  reservePercent?: number
  windowHours?: number
  default?: QuotaRule
  models?: Record<string, QuotaRule>
}

export interface RegistryUsage {
  windowHours: number
  totalCostUsd?: number
  totalRequests?: number
}

export interface ModelRegistry {
  models: RegistryModel[]
  chain: string[]
  chains: Record<string, string[]>
  fallback: FallbackPolicy
  agentNotes: Record<string, Record<string, string>>
  quota: QuotaConfig
  usage: RegistryUsage
  modelHealth: ModelHealth[]
  keyHealth: KeyHealth[]
}

/** The `models` config block subset the PUT accepts; omitted keys are left untouched. */
export interface ModelsConfigPatch {
  chain?: string[]
  chains?: Record<string, string[]>
  fallback?: Partial<FallbackPolicy>
  notes?: Record<string, string>
  agentNotes?: Record<string, Record<string, string>>
  hidden?: string[]
  favorites?: string[]
  pricing?: Record<string, ModelCost>
  quota?: QuotaConfig
}

export type AgentMode = "primary" | "subagent" | "all"

/** The agent fields the policy editor reads from `GET /api/agent`. */
export interface AgentOption {
  id: string
  name: string
  mode: AgentMode
  hidden: boolean
  model?: { providerID: string; id: string }
}

export type RegistrySnapshot =
  | { available: true; registry: ModelRegistry }
  | { available: false; message: string }

export class ModelsSaveError extends Error {
  readonly status: number

  constructor(status: number, message: string) {
    super(message)
    this.name = "ModelsSaveError"
    this.status = status
  }
}

const isRecord = (value: unknown): value is Record<string, unknown> =>
  !!value && typeof value === "object" && !Array.isArray(value)

const text = (value: unknown): string | undefined => (typeof value === "string" ? value : undefined)
const number = (value: unknown): number | undefined =>
  typeof value === "number" && Number.isFinite(value) ? value : undefined
const boolean = (value: unknown): boolean | undefined => (typeof value === "boolean" ? value : undefined)
const strings = (value: unknown): string[] =>
  Array.isArray(value) ? value.filter((item): item is string => typeof item === "string") : []

/** Unwraps the shared `{ "data": ... }` envelope. */
function payload(value: unknown): unknown {
  return isRecord(value) && "data" in value ? value.data : value
}

function parseCost(value: unknown): ModelCost | undefined {
  if (!isRecord(value)) return undefined
  const input = number(value.input)
  const output = number(value.output)

  if (input === undefined || output === undefined) return undefined
  const cache = isRecord(value.cache)
    ? { read: number(value.cache.read), write: number(value.cache.write) }
    : undefined

  return cache ? { input, output, cache } : { input, output }
}

function parseQuota(value: unknown): ModelQuota | undefined {
  if (!isRecord(value)) return undefined
  const quota: ModelQuota = {}
  const maxCostUsd = number(value.maxCostUsd)
  const maxRequests = number(value.maxRequests)
  const usedCostUsd = number(value.usedCostUsd)
  const usedRequests = number(value.usedRequests)
  const remainingPercent = number(value.remainingPercent)
  const limited = boolean(value.limited)

  if (maxCostUsd !== undefined) quota.maxCostUsd = maxCostUsd
  if (maxRequests !== undefined) quota.maxRequests = maxRequests
  if (usedCostUsd !== undefined) quota.usedCostUsd = usedCostUsd
  if (usedRequests !== undefined) quota.usedRequests = usedRequests
  if (remainingPercent !== undefined) quota.remainingPercent = remainingPercent
  if (limited !== undefined) quota.limited = limited

  return Object.keys(quota).length > 0 ? quota : undefined
}

function parseUsage(value: unknown): ModelUsage | undefined {
  if (!isRecord(value)) return undefined
  const windowHours = number(value.windowHours)

  if (windowHours === undefined) return undefined
  const usage: ModelUsage = { requests: number(value.requests) ?? 0, windowHours }

  const input = number(value.tokensInput)
  const output = number(value.tokensOutput)
  const reasoning = number(value.tokensReasoning)
  const cost = number(value.costUsd)

  if (input !== undefined) usage.tokensInput = input
  if (output !== undefined) usage.tokensOutput = output
  if (reasoning !== undefined) usage.tokensReasoning = reasoning
  if (cost !== undefined) usage.costUsd = cost

  return usage
}

function parseUnavailable(value: unknown): ModelUnavailable | undefined {
  if (!isRecord(value)) return undefined
  const untilMs = number(value.untilMs)
  const kind = text(value.kind)

  if (untilMs === undefined || !kind) return undefined
  const message = text(value.message)

  return message ? { untilMs, kind, message } : { untilMs, kind }
}

function parseModel(value: unknown): RegistryModel | undefined {
  if (!isRecord(value)) return undefined
  const providerID = text(value.providerID)
  const id = text(value.id)
  const name = text(value.name)

  if (!providerID || !id || !name) return undefined

  const model: RegistryModel = {
    providerID,
    id,
    name,
    visible: boolean(value.visible) ?? true,
    favorite: boolean(value.favorite) ?? false,
  }

  const variants = strings(value.variants)
  if (variants.length > 0) model.variants = variants
  const cost = Array.isArray(value.cost)
    ? value.cost.flatMap((item) => {
        const parsed = parseCost(item)

        return parsed ? [parsed] : []
      })
    : []
  if (cost.length > 0) model.cost = cost
  const limit = isRecord(value.limit)
    ? { context: number(value.limit.context), input: number(value.limit.input), output: number(value.limit.output) }
    : undefined
  if (limit && Object.values(limit).some((item) => item !== undefined)) model.limit = limit
  const note = text(value.note)
  if (note) model.note = note
  const usage = parseUsage(value.usage)
  if (usage) model.usage = usage
  const quota = parseQuota(value.quota)
  if (quota) model.quota = quota
  const unavailable = parseUnavailable(value.unavailable)
  if (unavailable) model.unavailable = unavailable

  return model
}

function parseModelHealth(value: unknown): ModelHealth[] {
  if (!Array.isArray(value)) return []

  return value.flatMap((item): ModelHealth[] => {
    if (!isRecord(item)) return []
    const providerID = text(item.providerID)
    const id = text(item.id)
    const reference = text(item.reference) ?? (providerID && id ? `${providerID}/${id}` : undefined)
    const untilMs = number(item.untilMs)
    const kind = text(item.kind)

    if (!providerID || !id || !reference || untilMs === undefined || !kind) return []
    const message = text(item.message)

    return [message ? { providerID, id, reference, untilMs, kind, message } : { providerID, id, reference, untilMs, kind }]
  })
}

function parseKeyHealth(value: unknown): KeyHealth[] {
  if (!Array.isArray(value)) return []

  return value.flatMap((item): KeyHealth[] => {
    if (!isRecord(item)) return []
    const providerID = text(item.providerID)
    const untilMs = number(item.untilMs)
    const kind = text(item.kind)

    if (!providerID || untilMs === undefined || !kind) return []
    const credentialID = text(item.credentialID)
    // The server always publishes the counter; a body without one still shows
    // the entry, with the count it actually reported.
    const health: KeyHealth = { providerID, untilMs, kind, failures: number(item.failures) ?? 0 }

    // Absent means the environment key: never invent a credential id for it.
    if (credentialID) health.credentialID = credentialID

    return [health]
  })
}

function parseNoteMap(value: unknown): Record<string, Record<string, string>> {
  if (!isRecord(value)) return {}
  const result: Record<string, Record<string, string>> = {}

  for (const [agent, entries] of Object.entries(value)) {
    if (!isRecord(entries)) continue
    const notes: Record<string, string> = {}

    for (const [ref, note] of Object.entries(entries)) if (typeof note === "string") notes[ref] = note
    if (Object.keys(notes).length > 0) result[agent] = notes
  }

  return result
}

function parseQuotaRule(value: unknown): QuotaRule | undefined {
  if (!isRecord(value)) return undefined
  const rule: QuotaRule = {}

  const cost = number(value.maxCostUsd)
  const requests = number(value.maxRequests)

  if (cost !== undefined) rule.maxCostUsd = cost
  if (requests !== undefined) rule.maxRequests = requests

  return Object.keys(rule).length > 0 ? rule : undefined
}

function parseQuotaConfig(value: unknown): QuotaConfig {
  if (!isRecord(value)) return {}
  const quota: QuotaConfig = {}

  const reserve = number(value.reservePercent)
  const windowHours = number(value.windowHours)

  if (reserve !== undefined) quota.reservePercent = reserve
  if (windowHours !== undefined) quota.windowHours = windowHours
  const fallback = parseQuotaRule(value.default)

  if (fallback) quota.default = fallback
  if (isRecord(value.models)) {
    const models: Record<string, QuotaRule> = {}

    for (const [ref, rule] of Object.entries(value.models)) {
      const parsed = parseQuotaRule(rule)

      if (parsed) models[ref] = parsed
    }

    if (Object.keys(models).length > 0) quota.models = models
  }

  return quota
}

function parseChainMap(value: unknown): Record<string, string[]> {
  if (!isRecord(value)) return {}
  const chains: Record<string, string[]> = {}

  for (const [agent, chain] of Object.entries(value)) chains[agent] = strings(chain)

  return chains
}

/** Parses a registry body (bare or `{ data: ... }`), or `undefined` when it cannot be trusted. */
export function parseRegistry(value: unknown): ModelRegistry | undefined {
  const data = payload(value)

  if (!isRecord(data) || !Array.isArray(data.models)) return undefined
  const models = data.models.flatMap((item) => {
    const parsed = parseModel(item)

    return parsed ? [parsed] : []
  })
  const fallback = isRecord(data.fallback) ? data.fallback : {}
  const usage = isRecord(data.usage) ? data.usage : {}
  const windowHours = number(usage.windowHours) ?? number(parseQuotaConfig(data.quota).windowHours) ?? 0
  const registry: ModelRegistry = {
    models,
    chain: strings(data.chain),
    chains: parseChainMap(data.chains),
    fallback: {
      on: strings(fallback.on),
      maxHops: number(fallback.maxHops) ?? 0,
    },
    agentNotes: parseNoteMap(data.agentNotes),
    quota: parseQuotaConfig(data.quota),
    usage: {
      windowHours,
      totalCostUsd: number(usage.totalCostUsd),
      totalRequests: number(usage.totalRequests),
    },
    modelHealth: parseModelHealth(data.modelHealth),
    keyHealth: parseKeyHealth(data.keyHealth),
  }

  return registry
}

const agentMode = (value: unknown): AgentMode =>
  value === "primary" || value === "subagent" || value === "all" ? value : "all"

/** The agent list the server exposes; unknown fields are ignored. */
export function parseAgents(value: unknown): AgentOption[] {
  const data = payload(value)

  if (!Array.isArray(data)) return []
  const agents = data.flatMap((item): AgentOption[] => {
    if (!isRecord(item)) return []
    const id = text(item.id)
    const name = text(item.name) ?? id

    if (!id || !name) return []
    const model = isRecord(item.model) ? item.model : undefined
    const providerID = model ? text(model.providerID) : undefined
    const modelID = model ? text(model.id) : undefined

    return [
      {
        id,
        name,
        mode: agentMode(item.mode),
        hidden: boolean(item.hidden) ?? false,
        ...(providerID && modelID ? { model: { providerID, id: modelID } } : {}),
      },
    ]
  })

  return agents.toSorted((a, b) => a.name.localeCompare(b.name))
}

/**
 * `?usage=0` omits the per-model usage rollup (`docs/15-performance.md`): the
 * Models tab paints sparse and refetches with usage only when the usage section
 * is opened. An absent or `1` value keeps the full body.
 */
export const registryPath = (includeUsage: boolean): string =>
  includeUsage ? "/api/models/registry" : "/api/models/registry?usage=0"

/** One cache entry per body shape, so the two paints never overwrite each other. */
export const registryQueryKey = (scope: string, includeUsage: boolean): unknown[] => [
  scope,
  "models-registry",
  includeUsage ? "usage" : "sparse",
]

export interface UseModelsRegistryOptions {
  /** Reads reactively: `false` requests the sparse body, `true` (default) the usage rollup. */
  usage?: () => boolean
}

export function useModelsRegistry(options: UseModelsRegistryOptions = {}) {
  const sdk = useServerSDK()
  const client = useQueryClient()
  const request = useAgenticRequest()
  const usage = () => options.usage?.() ?? true

  const query = useQuery<RegistrySnapshot>(() => ({
    queryKey: registryQueryKey(sdk.scope, usage()),
    queryFn: async (): Promise<RegistrySnapshot> => {
      const response = await request(registryPath(usage()))

      // The route is landing in a parallel wave; keep the tab usable and say so.
      if (response.status === 404) return { available: false, message: await serverMessage(response) }
      if (!response.ok) throw new ModelsSaveError(response.status, await serverMessage(response))
      const registry = parseRegistry(await response.json())

      if (!registry) throw new Error("Model registry response could not be parsed")

      return { available: true, registry }
    },
    // Switching between the sparse and the usage body keeps the tab painted.
    placeholderData: (previous) => previous,
    staleTime: 30_000,
    refetchOnMount: false,
    retry: false,
  }))

  const save = async (patch: ModelsConfigPatch): Promise<{ registry?: ModelRegistry }> => {
    const response = await request("/api/config/models", { method: "PUT", body: JSON.stringify(patch) })

    if (!response.ok) throw new ModelsSaveError(response.status, await serverMessage(response))
    const registry = parseRegistry(await response.json())

    await client.invalidateQueries({ queryKey: [sdk.scope, "models-registry"] })

    return { registry }
  }

  return { query, save }
}

/** Agent names for the scope selector; a missing route simply yields an empty list. */
export function useAgentOptions() {
  const sdk = useServerSDK()
  const request = useAgenticRequest()

  return useQuery<AgentOption[]>(() => ({
    queryKey: [sdk.scope, "models-agents"],
    queryFn: async (): Promise<AgentOption[]> => {
      const response = await request("/api/agent")

      if (!response.ok) return []

      return parseAgents(await response.json())
    },
    staleTime: 30_000,
    refetchOnMount: false,
    retry: false,
  }))
}

export const modelRef = (providerID: string, id: string) => `${providerID}/${id}`

export const parseModelRef = (value: string): { providerID: string; id: string } | undefined => {
  const index = value.indexOf("/")

  if (index <= 0 || index === value.length - 1) return undefined

  return { providerID: value.slice(0, index), id: value.slice(index + 1) }
}
