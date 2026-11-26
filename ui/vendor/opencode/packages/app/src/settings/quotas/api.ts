import { useQuery, useQueryClient } from "@tanstack/solid-query"
import { useServerSDK } from "@/runtime/server/client"
import { isConfigObject, serverMessage, useAgenticRequest } from "@/settings/agentic/config"

export type QuotaCapability = "live_api" | "rate_limit_headers_only"

/** Where the OpenRouter management key the server uses comes from. */
export type ManagementKeySource = "env" | "credential" | "config"

export interface UnifiedQuotaSummary {
  providerCount: number
  liveQuotaCount: number
  spend5hUsd: number
  spend24hUsd: number
  spend7dUsd: number
  spend30dUsd: number
}

export interface QuotaWindow {
  id: string
  label: string
  unit: string
  groupKey?: string
  limit?: number
  used?: number
  remaining?: number
  usedPercent?: number
  remainingPercent?: number
  limited?: boolean
  resetAtMs?: number
  resetLabel?: string
}

export interface ProviderBalanceSummary {
  currency: string
  total?: number
  remaining?: number
  used?: number
  toppedUp?: number
  granted?: number
  isAvailable?: boolean
  isFreeTier?: boolean
}

export interface ProviderApiSpend {
  currency: string
  total?: number
  daily?: number
  weekly?: number
  monthly?: number
}

export interface ProviderRateLimit {
  providerId: string
  modelId: string
  observedAtMs: number
  status: number
  requestsLimit?: number
  requestsRemaining?: number
  requestsResetAtMs?: number
  requestsResetRaw?: string
  tokensLimit?: number
  tokensRemaining?: number
  tokensResetAtMs?: number
  tokensResetRaw?: string
  retryAfterMs?: number
}

export interface AntigravityQuotaGroupSnapshot {
  remainingFraction?: number
  usedPercent?: number
  resetTimeMs?: number
  label?: string
  models?: string[]
}

export interface AntigravityCachedQuota {
  updatedAt: number
  groups: Record<string, AntigravityQuotaGroupSnapshot>
}

export interface AntigravityAccountStatus {
  index: number
  id: string
  email?: string
  label?: string
  activeClaude: boolean
  activeGemini: boolean
  enabled: boolean
  verificationRequired: boolean
  verificationUrl?: string
  revoked: boolean
  projectId?: string
  tier?: string
  quota?: AntigravityCachedQuota
  /**
   * The latest still-future rate-limit deadline across this account's quota
   * keys. The server omits it for a healthy account, never `null`.
   */
  cooldownUntilMs?: number
  addedAt: number
  lastUsed: number
}

/**
 * One stored credential of a provider, joined with its runtime rotation state.
 *
 * `active` is the stored selection; a request tries `keys[0]` first. The
 * server keeps a cooling key in rotation order rather than hiding it, so the
 * list always shows every key the provider owns.
 *
 * `usedCostUsd` / `usedRequests` are always `0` by design: the usage rollup
 * keys on the model's provider alone, so per-key attribution does not exist.
 * They stay on the wire for a future column and are deliberately NOT rendered.
 */
export interface ProviderKeyEntry {
  credentialID: string
  label: string
  active: boolean
  limited: boolean
  cooldownUntilMs?: number
  kind?: string
  failures?: number
  usedCostUsd?: number
  usedRequests?: number
  status?: string
  fetchedAtMs?: number
  cached?: boolean
  windows?: QuotaWindow[]
  balances?: ProviderBalanceSummary[]
  spendSummary?: ProviderApiSpend
  warnings?: string[]
  error?: string
}

export interface UsageWindowTotals {
  steps: number
  inputTokens: number
  outputTokens: number
  reasoningTokens: number
  cacheReadTokens: number
  cacheWriteTokens: number
  totalTokens: number
  costUsd: number
}

export interface ModelUsageRow {
  modelID: string
  steps: number
  totalTokens: number
  costUsd: number
}

export interface ProviderLocalUsage {
  fiveHours: UsageWindowTotals
  day: UsageWindowTotals
  week: UsageWindowTotals
  month: UsageWindowTotals
  allTime: UsageWindowTotals
  topModels?: ModelUsageRow[]
}

export interface ProviderQuotaEntry {
  providerID: string
  name: string
  connected: boolean
  connectionSource: string
  capability: QuotaCapability
  capabilityNote?: string
  status: string
  fetchedAtMs?: number
  cached?: boolean
  hasManagementKey?: boolean
  managementKeySource?: ManagementKeySource
  keyLabel?: string
  rateLimitRule?: string
  windows?: QuotaWindow[]
  balances?: ProviderBalanceSummary[]
  spendSummary?: ProviderApiSpend
  rateLimit?: ProviderRateLimit
  accounts?: AntigravityAccountStatus[]
  /** Stored credentials in rotation order; omitted when the provider owns none. */
  keys?: ProviderKeyEntry[]
  localUsage?: ProviderLocalUsage
  warnings?: string[]
  error?: string
}

export interface UnifiedQuotasResponse {
  fetchedAtMs: number
  summary: UnifiedQuotaSummary
  providers: ProviderQuotaEntry[]
  /**
   * The server served the cache and is re-probing providers in the background.
   *
   * Optional on the wire: the server omits it when nothing is refreshing, and
   * older servers never sent it. `parseQuotasResponse` normalizes it to a
   * boolean, so a payload object literal may omit it too.
   */
  refreshing?: boolean
}

/** The server answers `503` when it runs without the quota service. */
export interface QuotasUnavailable {
  unavailable: true
  message?: string
}

export type QuotasSnapshot = UnifiedQuotasResponse | QuotasUnavailable

const isRecord = (value: unknown): value is Record<string, unknown> =>
  !!value && typeof value === "object" && !Array.isArray(value)

const optionalNumber = (value: unknown): number | undefined =>
  typeof value === "number" && Number.isFinite(value) ? value : undefined

/**
 * Parses `provider.keys`. The field is optional and absent for a provider with
 * no stored credentials, so an absent or malformed list yields `[]` rather than
 * a placeholder key. An entry without a `credentialID` cannot be acted on and is
 * dropped instead of being shown with an invented id.
 */
export function parseProviderKeys(value: unknown): ProviderKeyEntry[] {
  if (!Array.isArray(value)) return []

  return value.flatMap((item): ProviderKeyEntry[] => {
    if (!isRecord(item)) return []
    const credentialID = typeof item.credentialID === "string" ? item.credentialID : undefined

    if (!credentialID) return []
    const key: ProviderKeyEntry = {
      credentialID,
      label: typeof item.label === "string" && item.label.length > 0 ? item.label : credentialID,
      active: item.active === true,
      limited: item.limited === true,
    }
    const cooldownUntilMs = optionalNumber(item.cooldownUntilMs)
    const kind = typeof item.kind === "string" ? item.kind : undefined
    const failures = optionalNumber(item.failures)
    const usedCostUsd = optionalNumber(item.usedCostUsd)
    const usedRequests = optionalNumber(item.usedRequests)
    const status = typeof item.status === "string" && item.status.length > 0 ? item.status : undefined
    const fetchedAtMs = optionalNumber(item.fetchedAtMs)
    const cached = typeof item.cached === "boolean" ? item.cached : undefined
    const windows = Array.isArray(item.windows)
      ? (item.windows.filter(isRecord) as unknown as QuotaWindow[])
      : undefined
    const balances = Array.isArray(item.balances)
      ? (item.balances.filter(isRecord) as unknown as ProviderBalanceSummary[])
      : undefined
    const spendSummary = isRecord(item.spendSummary)
      ? (item.spendSummary as unknown as ProviderApiSpend)
      : undefined
    const warnings = Array.isArray(item.warnings)
      ? item.warnings.filter((w): w is string => typeof w === "string" && w.length > 0)
      : undefined
    const error = typeof item.error === "string" && item.error.length > 0 ? item.error : undefined

    if (cooldownUntilMs !== undefined) key.cooldownUntilMs = cooldownUntilMs
    if (kind) key.kind = kind
    if (failures !== undefined) key.failures = failures
    if (usedCostUsd !== undefined) key.usedCostUsd = usedCostUsd
    if (usedRequests !== undefined) key.usedRequests = usedRequests
    if (status) key.status = status
    if (fetchedAtMs !== undefined) key.fetchedAtMs = fetchedAtMs
    if (cached !== undefined) key.cached = cached
    if (windows && windows.length > 0) key.windows = windows
    if (balances && balances.length > 0) key.balances = balances
    if (spendSummary) key.spendSummary = spendSummary
    if (warnings && warnings.length > 0) key.warnings = warnings
    if (error) key.error = error

    return [key]
  })
}

/**
 * Parses the optional per-account `cooldownUntilMs`. The server omits it for a
 * healthy account, and a cooling deadline is only meaningful while it is in the
 * future, so a past deadline is dropped instead of being shown as "resetting".
 */
function parseAccountCooldown(value: unknown, nowMs: number): number | undefined {
  const until = optionalNumber(value)

  return until !== undefined && until > nowMs ? until : undefined
}

/**
 * Normalizes a quota response body. Only the fields this client must not trust
 * blindly are parsed: `keys` (optional per provider) and the antigravity
 * `cooldownUntilMs` (optional per account). Everything else keeps the server's
 * shape, because the panel renders it verbatim.
 */
export function parseQuotasResponse(value: unknown, nowMs = Date.now()): UnifiedQuotasResponse | undefined {
  if (!isRecord(value)) return undefined
  if (!Array.isArray(value.providers)) return undefined
  if (!isRecord(value.summary)) return undefined

  const providers = value.providers.flatMap((item): ProviderQuotaEntry[] => {
    if (!isRecord(item)) return []
    const providerID = typeof item.providerID === "string" ? item.providerID : undefined

    if (!providerID) return []
    const keys = parseProviderKeys(item.keys)
    const accounts = Array.isArray(item.accounts)
      ? item.accounts.flatMap((account): AntigravityAccountStatus[] => {
          if (!isRecord(account)) return []
          const cooldownUntilMs = parseAccountCooldown(account.cooldownUntilMs, nowMs)
          const parsed = { ...account, cooldownUntilMs } as unknown as AntigravityAccountStatus

          if (cooldownUntilMs === undefined) delete parsed.cooldownUntilMs

          return [parsed]
        })
      : undefined
    const provider = { ...item, providerID } as unknown as ProviderQuotaEntry

    if (keys.length > 0) provider.keys = keys
    else delete provider.keys
    if (accounts) provider.accounts = accounts

    return [provider]
  })

  return {
    ...(value as unknown as UnifiedQuotasResponse),
    fetchedAtMs: optionalNumber(value.fetchedAtMs) ?? nowMs,
    providers,
    // A cache-first payload says so; an older server that omits the field is
    // not refreshing as far as this client can tell.
    refreshing: value.refreshing === true,
  }
}

/** True while the server is re-probing providers behind a cached payload. */
export function quotasRefreshing(value: QuotasSnapshot | undefined): boolean {
  return quotasData(value)?.refreshing === true
}

/** The unified payload, or `undefined` while pending or when the service is detached. */
export function quotasData(value: QuotasSnapshot | undefined): UnifiedQuotasResponse | undefined {
  return value && !("unavailable" in value) ? value : undefined
}

export function quotasUnavailable(value: QuotasSnapshot | undefined): QuotasUnavailable | undefined {
  return value && "unavailable" in value ? value : undefined
}

/** A failed quota request, with the server's error kind when it names one. */
export class QuotaRequestError extends Error {
  readonly status: number
  readonly kind: string | undefined

  constructor(status: number, message: string, kind?: string) {
    super(message)
    this.name = "QuotaRequestError"
    this.status = status
    this.kind = kind
  }
}

/** The server's status vocabulary for a provider entry. */
export const QUOTA_STATUSES = [
  "ok",
  "partial",
  "error",
  "unsupported",
  "headers_only",
  "local_only",
  "no_api_key",
  "not_connected",
  "disconnected",
  "unavailable",
] as const

export type QuotaStatusLabelKey =
  | "settings.quotas.status.ok"
  | "settings.quotas.status.partial"
  | "settings.quotas.status.error"
  | "settings.quotas.status.unsupported"
  | "settings.quotas.status.headersOnly"
  | "settings.quotas.status.localOnly"
  | "settings.quotas.status.noApiKey"
  | "settings.quotas.status.notConnected"
  | "settings.quotas.status.disconnected"
  | "settings.quotas.status.unavailable"

/** Every server status maps to a localized label; unknown tokens never reach the UI. */
export function statusLabelKey(status: string): QuotaStatusLabelKey {
  switch (status) {
    case "ok":
      return "settings.quotas.status.ok"
    case "partial":
      return "settings.quotas.status.partial"
    case "error":
      return "settings.quotas.status.error"
    case "unsupported":
      return "settings.quotas.status.unsupported"
    case "headers_only":
      return "settings.quotas.status.headersOnly"
    case "local_only":
      return "settings.quotas.status.localOnly"
    case "no_api_key":
      return "settings.quotas.status.noApiKey"
    case "not_connected":
      return "settings.quotas.status.notConnected"
    case "disconnected":
      return "settings.quotas.status.disconnected"
    default:
      return "settings.quotas.status.unavailable"
  }
}

const QUOTAS_QUERY_KEY = ["quotas", "unified"] as const

/** The failure classes a cooling key can report, in the server's wire vocabulary. */
export type KeyKindLabelKey =
  | "settings.quotas.keys.kind.quota"
  | "settings.quotas.keys.kind.rate_limit"
  | "settings.quotas.keys.kind.auth"

/**
 * Maps a key's failure `kind` to a localized badge label. The server may report
 * classes this panel has no badge for (transport, unknown, …); those fall back
 * to the generic "limited" wording rather than leaking a raw token.
 */
export function keyKindLabelKey(kind: string | undefined): KeyKindLabelKey | undefined {
  switch (kind) {
    case "quota":
    case "quota_exhausted":
      return "settings.quotas.keys.kind.quota"
    case "rate_limit":
      return "settings.quotas.keys.kind.rate_limit"
    case "auth":
      return "settings.quotas.keys.kind.auth"
    default:
      return undefined
  }
}

export interface UseQuotasDataOptions {
  /** Reads reactively; a detached dialog passes `() => false` until it needs the data. */
  enabled?: () => boolean
  /** Poll interval in ms. Only the settings panel polls; the dialog reads the cached snapshot. */
  refetchInterval?: number
}

/** Reads the error kind out of a failed response without consuming the body twice. */
async function requestError(response: Response): Promise<QuotaRequestError> {
  const copy = response.clone()
  const message = await serverMessage(response)
  let kind: string | undefined

  try {
    const body: unknown = await copy.json()
    if (isConfigObject(body)) {
      const data = body.data
      if (isConfigObject(data) && typeof data.kind === "string") kind = data.kind
      else if (typeof body.kind === "string") kind = body.kind
    }
  } catch {
    // Not JSON.
  }

  return new QuotaRequestError(response.status, message, kind)
}

export function useQuotasData(options: UseQuotasDataOptions = {}) {
  const client = useQueryClient()
  const sdk = useServerSDK()
  const request = useAgenticRequest()

  const query = useQuery<QuotasSnapshot>(() => ({
    queryKey: [sdk.scope, ...QUOTAS_QUERY_KEY],
    queryFn: async (): Promise<QuotasSnapshot> => {
      const response = await request("/api/quotas")
      // A server without the quota service is a soft state, not an error.
      if (response.status === 503) {
        return { unavailable: true, message: await serverMessage(response) }
      }
      if (!response.ok) throw await requestError(response)
      const payload = (await response.json()) as { data?: unknown }
      const data = parseQuotasResponse(payload.data)

      if (!data) throw new Error("Missing quotas response data")
      if (data.refreshing && options.refetchInterval) {
        setTimeout(() => {
          void client.invalidateQueries({ queryKey: [sdk.scope, ...QUOTAS_QUERY_KEY] })
        }, 1_500)
      }

      return data
    },
    staleTime: 30_000,
    enabled: options.enabled?.() ?? true,
    retry: false,
    refetchInterval: options.refetchInterval,
  }))

  const refreshLive = async () => {
    const response = await request("/api/quotas/refresh", { method: "POST" })
    if (!response.ok) throw await requestError(response)
    const payload = (await response.json()) as { data?: unknown }
    const data = parseQuotasResponse(payload.data)

    if (data) {
      client.setQueryData([sdk.scope, ...QUOTAS_QUERY_KEY], data)
    } else {
      await client.invalidateQueries({ queryKey: [sdk.scope, ...QUOTAS_QUERY_KEY] })
    }
    return data
  }

  const invalidate = () => client.invalidateQueries({ queryKey: [sdk.scope, ...QUOTAS_QUERY_KEY] })

  const saveOpenRouterManagementKey = async (key: string) => {
    const response = await request("/api/quotas/openrouter/management-key", {
      method: "PUT",
      body: JSON.stringify({ key }),
    })
    if (!response.ok) throw await requestError(response)
    // One invalidation re-reads the cache; a live probe of every provider is reserved for Refresh.
    await invalidate()
  }

  const clearOpenRouterManagementKey = async () => {
    const response = await request("/api/quotas/openrouter/management-key", {
      method: "DELETE",
    })
    if (!response.ok) throw await requestError(response)
    await invalidate()
  }

  const mutateAntigravityAccount = async (id: string, action: "select" | "enable" | "disable") => {
    const response = await request(`/api/antigravity/accounts/${encodeURIComponent(id)}/${action}`, {
      method: "POST",
    })
    if (!response.ok) throw await requestError(response)
    await invalidate()
  }

  return {
    query,
    refreshLive,
    invalidate,
    saveOpenRouterManagementKey,
    clearOpenRouterManagementKey,
    mutateAntigravityAccount,
  }
}

export function formatCurrencyValue(value: number, currency = "USD", locale?: string): string {
  const code = /^[A-Z]{3}$/.test(currency.toUpperCase()) ? currency.toUpperCase() : "USD"
  return new Intl.NumberFormat(locale, {
    style: "currency",
    currency: code,
    minimumFractionDigits: Math.abs(value) > 0 && Math.abs(value) < 0.01 ? 4 : 2,
    maximumFractionDigits: Math.abs(value) > 0 && Math.abs(value) < 0.01 ? 4 : 2,
  }).format(value)
}

export function formatCompactNumber(value: number, locale?: string): string {
  if (Math.abs(value) >= 1_000_000) {
    return `${(value / 1_000_000).toFixed(2)}M`
  }
  if (Math.abs(value) >= 1_000) {
    return `${(value / 1_000).toFixed(1)}K`
  }
  return new Intl.NumberFormat(locale).format(value)
}

export function formatCountdownDuration(targetMs: number, nowMs = Date.now()): string | undefined {
  const diff = targetMs - nowMs
  if (!Number.isFinite(diff) || diff <= 0) return undefined
  const totalMinutes = Math.ceil(diff / 60_000)
  const days = Math.floor(totalMinutes / (60 * 24))
  const hours = Math.floor((totalMinutes % (60 * 24)) / 60)
  const mins = totalMinutes % 60
  if (days > 0) return `${days}d ${hours}h`
  if (hours > 0) return `${hours}h ${mins}m`
  return `${mins}m`
}

export function formatQuotaWindowValue(window: QuotaWindow, value: number, locale?: string): string {
  if (window.unit === "percent") return `${Math.round(value)}%`
  if (/^[A-Z]{3}$/.test(window.unit.toUpperCase())) {
    return formatCurrencyValue(value, window.unit, locale)
  }
  return `${formatCompactNumber(value, locale)} ${window.unit}`
}

export function antigravityPoolRemaining(group: AntigravityQuotaGroupSnapshot): number | undefined {
  if (typeof group.remainingFraction === "number") {
    const pct = group.remainingFraction <= 1 ? group.remainingFraction * 100 : group.remainingFraction
    return Math.max(0, Math.min(100, pct))
  }
  if (typeof group.usedPercent === "number") {
    return Math.max(0, Math.min(100, 100 - group.usedPercent))
  }
  return undefined
}
