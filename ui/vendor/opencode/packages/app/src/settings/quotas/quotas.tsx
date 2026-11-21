import { createMemo, createSignal, For, Show, type Component } from "solid-js"
import { Badge } from "@opencode/ui/badge"
import { Button } from "@opencode/ui/button"
import { Collapsible } from "@opencode/ui/collapsible"
import { Icon } from "@opencode/ui/icon"
import { Loader } from "@opencode/ui/loader"
import { ProviderIcon } from "@opencode/ui/provider-icon"
import { TextInput } from "@opencode/ui/text-input"
import { useLanguage } from "@/runtime/i18n/language"
import { showToast } from "@/shell/notifications/toast"
import {
  antigravityPoolRemaining,
  formatCompactNumber,
  formatCountdownDuration,
  formatCurrencyValue,
  formatQuotaWindowValue,
  keyKindLabelKey,
  QuotaRequestError,
  quotasData,
  quotasRefreshing,
  quotasUnavailable,
  statusLabelKey,
  useQuotasData,
  type AntigravityAccountStatus,
  type ProviderKeyEntry,
  type ProviderLocalUsage,
  type ProviderQuotaEntry,
  type ProviderRateLimit,
  type QuotaWindow,
} from "./api"
import "./quotas.css"

const SETTINGS_QUOTAS_POLL_MS = 60_000

function sourceLabelKey(source: string) {
  switch (source) {
    case "oauth":
      return "settings.quotas.source.oauth" as const
    case "credential":
      return "settings.quotas.source.credential" as const
    case "env":
      return "settings.quotas.source.env" as const
    case "config":
      return "settings.quotas.source.config" as const
    case "history":
      return "settings.quotas.source.history" as const
    default:
      return "settings.quotas.source.none" as const
  }
}

function windowFillPercent(window: QuotaWindow): number | undefined {
  if (typeof window.remainingPercent === "number") {
    return Math.max(0, Math.min(100, window.remainingPercent))
  }
  if (typeof window.usedPercent === "number") {
    return Math.max(0, Math.min(100, 100 - window.usedPercent))
  }
  if (typeof window.remaining === "number" && typeof window.limit === "number" && window.limit > 0) {
    return Math.max(0, Math.min(100, (window.remaining / window.limit) * 100))
  }
  return undefined
}

const WindowRow: Component<{ window: QuotaWindow }> = (props) => {
  const language = useLanguage()
  const fill = () => windowFillPercent(props.window)
  const limited = () => props.window.limited === true || (fill() !== undefined && fill()! <= 0)
  const warn = () => !limited() && fill() !== undefined && fill()! <= 20
  const resetText = () => {
    if (typeof props.window.resetAtMs === "number") {
      const countdown = formatCountdownDuration(props.window.resetAtMs)
      if (countdown) return language.t("settings.quotas.resetsIn", { duration: countdown })
    }
    if (props.window.resetLabel) return props.window.resetLabel
    return undefined
  }
  // Color alone never carries the state: a limited or nearly exhausted window also says so.
  const stateText = () => {
    if (limited()) return language.t("settings.quotas.window.limited")
    if (warn()) return language.t("settings.quotas.window.runningLow")
    return undefined
  }

  return (
    <div class="flex flex-col gap-1.5">
      <div class="flex flex-wrap items-baseline justify-between gap-2">
        <span class="text-12-medium text-v2-text-text-base">{props.window.label}</span>
        <div class="flex items-center gap-2 text-11-regular text-v2-text-text-muted">
          <Show when={typeof props.window.remaining === "number" && typeof props.window.limit === "number"}>
            <span class="text-v2-text-text-base">
              {formatQuotaWindowValue(props.window, props.window.remaining!, language.intl())} /{" "}
              {formatQuotaWindowValue(props.window, props.window.limit!, language.intl())}
            </span>
          </Show>
          <Show when={typeof props.window.remaining === "number" && typeof props.window.limit !== "number"}>
            <span class="text-v2-text-text-base">
              {formatQuotaWindowValue(props.window, props.window.remaining!, language.intl())}
            </span>
          </Show>
          <Show when={typeof props.window.usedPercent === "number"}>
            <span>({language.t("settings.quotas.percentUsed", { percent: Math.round(props.window.usedPercent!) })})</span>
          </Show>
          <Show when={resetText()}>{(text) => <span>· {text()}</span>}</Show>
          <Show when={stateText()}>
            {(text) => (
              <span data-testid="settings-quotas-window-state" data-state={limited() ? "limited" : "warn"}>
                {text()}
              </span>
            )}
          </Show>
        </div>
      </div>
      <Show when={fill() !== undefined}>
        <div
          class="quotas-bar-track"
          role="progressbar"
          aria-label={props.window.label}
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={Math.round(fill()!)}
          aria-valuetext={`${Math.round(fill()!)}%`}
          data-testid="settings-quotas-window-bar"
        >
          <div
            class="quotas-bar-fill"
            data-warn={warn()}
            data-limited={limited()}
            style={{ width: `${Math.round(fill()!)}%` }}
          />
        </div>
      </Show>
    </div>
  )
}

const RateLimitSection: Component<{ rateLimit: ProviderRateLimit }> = (props) => {
  const language = useLanguage()
  const reqReset = () =>
    typeof props.rateLimit.requestsResetAtMs === "number"
      ? formatCountdownDuration(props.rateLimit.requestsResetAtMs)
      : props.rateLimit.requestsResetRaw
  const tokReset = () =>
    typeof props.rateLimit.tokensResetAtMs === "number"
      ? formatCountdownDuration(props.rateLimit.tokensResetAtMs)
      : props.rateLimit.tokensResetRaw

  return (
    <div class="flex flex-col gap-1.5 rounded-md border border-v2-border-border-base bg-v2-background-bg-base px-3 py-2">
      <div class="flex flex-wrap items-center justify-between gap-2">
        <span class="text-12-medium text-v2-text-text-base">{language.t("settings.quotas.rateLimit.title")}</span>
        <span class="text-11-regular text-v2-text-text-muted">
          {props.rateLimit.modelId} · HTTP {props.rateLimit.status} ·{" "}
          {new Intl.DateTimeFormat(language.intl(), { timeStyle: "medium" }).format(props.rateLimit.observedAtMs)}
        </span>
      </div>
      <div class="flex flex-wrap gap-3 text-11-regular text-v2-text-text-muted">
        <Show
          when={
            typeof props.rateLimit.requestsLimit === "number" || typeof props.rateLimit.requestsRemaining === "number"
          }
        >
          <span>
            {language.t("settings.quotas.rateLimit.requests")}:{" "}
            <strong class="font-medium text-v2-text-text-base">
              {typeof props.rateLimit.requestsRemaining === "number"
                ? formatCompactNumber(props.rateLimit.requestsRemaining, language.intl())
                : "?"}
              {typeof props.rateLimit.requestsLimit === "number"
                ? ` / ${formatCompactNumber(props.rateLimit.requestsLimit, language.intl())}`
                : ""}
            </strong>
            <Show when={reqReset()}>{(r) => ` (${r()})`}</Show>
          </span>
        </Show>
        <Show
          when={typeof props.rateLimit.tokensLimit === "number" || typeof props.rateLimit.tokensRemaining === "number"}
        >
          <span>
            {language.t("settings.quotas.rateLimit.tokens")}:{" "}
            <strong class="font-medium text-v2-text-text-base">
              {typeof props.rateLimit.tokensRemaining === "number"
                ? formatCompactNumber(props.rateLimit.tokensRemaining, language.intl())
                : "?"}
              {typeof props.rateLimit.tokensLimit === "number"
                ? ` / ${formatCompactNumber(props.rateLimit.tokensLimit, language.intl())}`
                : ""}
            </strong>
            <Show when={tokReset()}>{(r) => ` (${r()})`}</Show>
          </span>
        </Show>
        <Show when={typeof props.rateLimit.retryAfterMs === "number"}>
          <span class="text-v2-state-fg-danger">
            {language.t("settings.quotas.rateLimit.retryAfter", {
              seconds: Math.ceil(props.rateLimit.retryAfterMs! / 1000),
            })}
          </span>
        </Show>
      </div>
    </div>
  )
}

const AntigravityAccountsSection: Component<{
  accounts: AntigravityAccountStatus[]
  busyId: string | undefined
  onAction: (id: string, action: "select" | "enable" | "disable") => void
}> = (props) => {
  const language = useLanguage()
  const poolEntries = (account: AntigravityAccountStatus) => {
    const groups = account.quota?.groups ?? {}
    const entries = Object.entries(groups)
    const hasDynamicPools = entries.some(([key, snap]) => key.startsWith("pool:") || Boolean(snap.label))
    const filtered = hasDynamicPools
      ? entries.filter(
          ([key, snap]) =>
            key.startsWith("pool:") || key.startsWith("gemini-cli") || Boolean(snap.label),
        )
      : entries
    return filtered.map(([name, snap]) => ({
      name: snap.label || name,
      remaining: antigravityPoolRemaining(snap),
      resetAtMs: snap.resetTimeMs,
    }))
  }
  // The server omits the deadline for a healthy account; a past one is already
  // reset and reads as healthy, so both render nothing.
  const accCooldown = (account: AntigravityAccountStatus) =>
    typeof account.cooldownUntilMs === "number"
      ? formatCountdownDuration(account.cooldownUntilMs)
      : undefined

  return (
    <div class="flex flex-col gap-2">
      <div class="text-12-medium text-v2-text-text-base">{language.t("settings.quotas.antigravity.accountsTitle")}</div>
      <div class="flex flex-col gap-2">
        <For each={props.accounts}>
          {(acc) => (
            <div
              data-testid={`settings-quotas-ag-account-${acc.index}`}
              class="flex flex-col gap-1.5 rounded-md border border-v2-border-border-base bg-v2-background-bg-base px-3 py-2"
            >
              <div class="flex flex-wrap items-center justify-between gap-2">
                <div class="flex min-w-0 flex-wrap items-center gap-1.5">
                  <span class="truncate text-12-medium text-v2-text-text-base">{acc.email || acc.label || acc.id}</span>
                  <Show when={acc.label && acc.email}>
                    <Badge variant="subtle">{acc.label}</Badge>
                  </Show>
                  <Show when={acc.activeClaude}>
                    <Badge variant="accent">Claude</Badge>
                  </Show>
                  <Show when={acc.activeGemini}>
                    <Badge variant="accent">Gemini</Badge>
                  </Show>
                  <Show when={!acc.enabled}>
                    <Badge variant="neutral">{language.t("settings.quotas.antigravity.disabled")}</Badge>
                  </Show>
                  <Show when={acc.revoked}>
                    <Badge variant="neutral">{language.t("settings.quotas.antigravity.revoked")}</Badge>
                  </Show>
                  <Show when={acc.verificationRequired}>
                    <Show
                      when={acc.verificationUrl}
                      fallback={<Badge variant="neutral">{language.t("settings.quotas.antigravity.verify")}</Badge>}
                    >
                      {(url) => (
                        <a
                          href={url()}
                          target="_blank"
                          rel="noopener noreferrer"
                          class="inline-flex items-center underline"
                        >
                          <Badge variant="accent">{language.t("settings.quotas.antigravity.verify")}</Badge>
                        </a>
                      )}
                    </Show>
                  </Show>
                  <Show when={accCooldown(acc)}>
                    {(text) => (
                      <Badge
                        variant="neutral"
                        data-testid={`settings-quotas-ag-cooldown-${acc.index}`}
                        data-state="cooldown"
                      >
                        {text()}
                      </Badge>
                    )}
                  </Show>
                </div>
                <div class="flex items-center gap-1.5">
                  <Show when={acc.enabled && !acc.revoked && (!acc.activeClaude || !acc.activeGemini)}>
                    <Button
                      size="small"
                      variant="outline"
                      disabled={props.busyId === acc.id}
                      onClick={() => props.onAction(acc.id, "select")}
                    >
                      {language.t("settings.quotas.antigravity.setActive")}
                    </Button>
                  </Show>
                  <Button
                    size="small"
                    variant="ghost-muted"
                    disabled={props.busyId === acc.id}
                    onClick={() => props.onAction(acc.id, acc.enabled ? "disable" : "enable")}
                  >
                    {acc.enabled
                      ? language.t("settings.quotas.antigravity.disable")
                      : language.t("settings.quotas.antigravity.enable")}
                  </Button>
                </div>
              </div>
              <Show when={poolEntries(acc).length > 0}>
                <div class="flex flex-wrap gap-3 text-11-regular text-v2-text-text-muted">
                  <For each={poolEntries(acc)}>
                    {(pool) => (
                      <span>
                        {pool.name}:{" "}
                        <strong class="font-medium text-v2-text-text-base">
                          {pool.remaining !== undefined ? `${Math.round(pool.remaining)}%` : "—"}
                        </strong>
                        <Show when={typeof pool.resetAtMs === "number" && formatCountdownDuration(pool.resetAtMs)}>
                          {(duration) => ` (${duration()})`}
                        </Show>
                      </span>
                    )}
                  </For>
                </div>
              </Show>
            </div>
          )}
        </For>
      </div>
    </div>
  )
}

const OpenRouterManagementSection: Component<{
  configured: boolean
  envManaged: boolean
  saving: boolean
  onSave: (key: string) => Promise<void>
  onClear: () => Promise<void>
}> = (props) => {
  const language = useLanguage()
  const [draft, setDraft] = createSignal("")

  const submit = async (e: SubmitEvent) => {
    e.preventDefault()
    const key = draft().trim()
    if (!key) return
    await props.onSave(key)
    setDraft("")
  }

  return (
    <form
      onSubmit={(e) => void submit(e)}
      class="flex flex-col gap-2 rounded-md border border-v2-border-border-base bg-v2-background-bg-base px-3 py-2"
    >
      <div class="flex flex-wrap items-center justify-between gap-2">
        <span class="text-12-medium text-v2-text-text-base">{language.t("settings.quotas.openrouter.mgmtTitle")}</span>
        <Badge variant={props.configured ? "accent" : "subtle"}>
          {props.configured
            ? language.t("settings.quotas.openrouter.mgmtConfigured")
            : language.t("settings.quotas.openrouter.mgmtMissing")}
        </Badge>
      </div>
      <Show when={props.envManaged}>
        <p
          data-testid="settings-quotas-openrouter-env-managed"
          class="m-0 text-11-regular text-v2-text-text-muted"
        >
          {language.t("settings.quotas.openrouter.envManaged")}
        </p>
      </Show>
      <div class="flex flex-wrap items-center gap-2">
        <div class="min-w-[200px] flex-1">
          <TextInput
            type="password"
            value={draft()}
            disabled={props.envManaged}
            onInput={(e) => setDraft(e.currentTarget.value)}
            placeholder={
              props.envManaged
                ? language.t("settings.quotas.openrouter.mgmtEnvPlaceholder")
                : language.t("settings.quotas.openrouter.mgmtPlaceholder")
            }
            aria-label={language.t("settings.quotas.openrouter.mgmtTitle")}
            data-testid="settings-quotas-openrouter-mgmt-input"
          />
        </div>
        <Button
          type="submit"
          size="small"
          variant="neutral"
          disabled={props.saving || props.envManaged || !draft().trim()}
        >
          {language.t("common.save")}
        </Button>
        <Show when={props.configured && !props.envManaged}>
          <Button
            type="button"
            size="small"
            variant="ghost-danger"
            disabled={props.saving}
            onClick={() => void props.onClear()}
          >
            {language.t("settings.quotas.openrouter.mgmtClear")}
          </Button>
        </Show>
      </div>
    </form>
  )
}

const LocalUsageSection: Component<{ usage: ProviderLocalUsage }> = (props) => {
  const language = useLanguage()
  const buckets = () => [
    { key: "5h", label: language.t("settings.quotas.window.fiveHours"), data: props.usage.fiveHours },
    { key: "24h", label: language.t("settings.quotas.window.day"), data: props.usage.day },
    { key: "7d", label: language.t("settings.quotas.window.week"), data: props.usage.week },
    { key: "30d", label: language.t("settings.quotas.window.month"), data: props.usage.month },
    { key: "all", label: language.t("settings.quotas.window.allTime"), data: props.usage.allTime },
  ]

  return (
    <div class="flex flex-col gap-2">
      <div class="text-12-medium text-v2-text-text-base">{language.t("settings.quotas.localUsage.title")}</div>
      <div class="quotas-window-grid">
        <For each={buckets()}>
          {(bucket) => (
            <div class="quotas-window-cell">
              <span class="text-11-regular text-v2-text-text-muted">{bucket.label}</span>
              <span class="text-12-medium text-v2-text-text-base">
                {formatCurrencyValue(bucket.data.costUsd, "USD", language.intl())}
              </span>
              <span class="text-11-regular text-v2-text-text-muted">
                {formatCompactNumber(bucket.data.totalTokens, language.intl())} tok · {bucket.data.steps}
              </span>
            </div>
          )}
        </For>
      </div>
      <Show when={props.usage.topModels && props.usage.topModels.length > 0}>
        <div class="flex flex-wrap items-center gap-1.5 text-11-regular text-v2-text-text-muted">
          <span>{language.t("settings.quotas.localUsage.topModels")}:</span>
          <For each={props.usage.topModels}>
            {(row) => (
              <Badge variant="subtle">
                {row.modelID} · {formatCompactNumber(row.totalTokens, language.intl())} ·{" "}
                {formatCurrencyValue(row.costUsd, "USD", language.intl())}
              </Badge>
            )}
          </For>
        </div>
      </Show>
    </div>
  )
}

const ProviderKeysSection: Component<{ keys: ProviderKeyEntry[] }> = (props) => {
  const language = useLanguage()

  return (
    <div class="flex flex-col gap-2" data-testid="settings-quotas-keys">
      <div class="text-12-medium text-v2-text-text-base">{language.t("settings.quotas.keys.title")}</div>
      <div class="flex flex-col gap-1.5">
        <For each={props.keys}>
          {(key) => {
            // The server reports a cooling key with the failure class that caused it.
            const kindLabel = () => {
              const labelKey = keyKindLabelKey(key.kind)

              return labelKey ? language.t(labelKey) : undefined
            }
            const resetsIn = () =>
              typeof key.cooldownUntilMs === "number" ? formatCountdownDuration(key.cooldownUntilMs) : undefined

            return (
              <div
                data-testid={`settings-quotas-key-${key.credentialID}`}
                data-limited={key.limited ? "true" : "false"}
                data-active={key.active ? "true" : "false"}
                class="flex flex-wrap items-center justify-between gap-2 rounded-md border border-v2-border-border-base bg-v2-background-bg-base px-3 py-1.5"
              >
                <div class="flex min-w-0 flex-wrap items-center gap-1.5">
                  <bdi class="min-w-0 truncate text-12-regular text-v2-text-text-base" title={key.credentialID}>
                    {key.label}
                  </bdi>
                  <Show when={key.active}>
                    <Badge variant="accent">{language.t("settings.quotas.keys.primary")}</Badge>
                  </Show>
                </div>
                <div class="flex shrink-0 flex-wrap items-center gap-2 text-11-regular text-v2-text-text-muted">
                  <Show when={key.limited}>
                    <Badge variant="neutral">
                      {kindLabel()
                        ? language.t("settings.quotas.keys.limitedWithKind", { kind: kindLabel()! })
                        : language.t("settings.quotas.keys.limited")}
                    </Badge>
                  </Show>
                  <Show when={resetsIn()}>
                    {(duration) => <span>{language.t("settings.quotas.keys.resetsIn", { duration: duration() })}</span>}
                  </Show>
                </div>
              </div>
            )
          }}
        </For>
      </div>
      {/*
        `usedCostUsd`/`usedRequests` are always 0 on the wire: per-key attribution
        does not exist yet, so a spend column here would report a fake zero.
      */}
    </div>
  )
}

/**
 * Per-provider compact row: requests, cost and remaining quota, expandable to
 * the per-key rotation rows and the rest of the provider's detail.
 *
 * The spend/request numbers come from `localUsage.day` (the honest 24h bucket);
 * `summary` totals are not repeated here because the block header owns them.
 */
const ProviderQuotaRow: Component<{
  entry: ProviderQuotaEntry
  open: boolean
  onToggle: () => void
  envManaged: boolean
  busyAccountId: string | undefined
  savingMgmt: boolean
  onAccountAction: (id: string, action: "select" | "enable" | "disable") => void
  onSaveMgmtKey: (key: string) => Promise<void>
  onClearMgmtKey: () => Promise<void>
}> = (props) => {
  const language = useLanguage()
  const open = () => props.open
  const e = () => props.entry
  const day = () => e().localUsage?.day
  const remaining = () => {
    const values = (e().windows ?? []).flatMap((window) => {
      const fill = windowFillPercent(window)

      return fill === undefined ? [] : [fill]
    })

    return values.length > 0 ? Math.min(...values) : undefined
  }
  const limited = () => (e().windows ?? []).some((window) => window.limited === true)

  return (
    <div data-testid={`settings-quotas-provider-${e().providerID}`} class="flex flex-col">
      <button
        type="button"
        class="flex w-full flex-wrap items-center gap-x-3 gap-y-1 rounded-md border border-v2-border-border-base bg-v2-background-bg-base px-3 py-2 text-start hover:bg-v2-overlay-simple-overlay-hover focus-visible:outline focus-visible:outline-2 focus-visible:outline-v2-border-border-focus"
        aria-expanded={open()}
        aria-label={language.t("settings.quotas.usage.expand", { provider: e().name })}
        onClick={() => props.onToggle()}
      >
        <span class={`shrink-0 text-v2-icon-icon-muted transition-transform ${open() ? "rotate-90" : ""}`}>
          <Icon name="chevron-right" size="small" />
        </span>
        <ProviderIcon id={e().providerID} class="size-4 shrink-0 text-v2-text-text-base" />
        <span class="min-w-0 flex-1 truncate text-13-regular text-v2-text-text-base">{e().name}</span>
        <Show when={limited()}>
          <Badge variant="neutral">{language.t("settings.quotas.window.limited")}</Badge>
        </Show>
        <Show when={e().keys && e().keys!.length > 1}>
          <Badge variant="subtle">{language.t("settings.quotas.usage.keys", { count: e().keys!.length })}</Badge>
        </Show>
        <span class="shrink-0 text-11-regular text-v2-text-text-muted">
          {language.t("settings.quotas.usage.requests", {
            requests: formatCompactNumber(day()?.steps ?? 0, language.intl()),
          })}
        </span>
        <span class="w-20 shrink-0 text-end text-11-regular text-v2-text-text-base">
          {formatCurrencyValue(day()?.costUsd ?? 0, "USD", language.intl())}
        </span>
        <span class="w-24 shrink-0 text-end text-11-regular text-v2-text-text-muted">
          <Show
            when={remaining() !== undefined}
            fallback={<span class="text-v2-text-text-faint">{language.t("settings.quotas.usage.noWindow")}</span>}
          >
            {language.t("settings.quotas.usage.remaining", { percent: Math.round(remaining()!) })}
          </Show>
        </span>
      </button>

      <Show when={open()}>
        <div class="mt-1 flex flex-col gap-3 rounded-md border border-v2-border-border-base bg-v2-background-bg-layer-01 px-3 py-2">
          <div class="flex flex-wrap items-center gap-2 text-11-regular text-v2-text-text-muted">
            <span>{language.t(sourceLabelKey(e().connectionSource))}</span>
            <Show when={e().rateLimitRule}>{(rule) => <span>· {rule()}</span>}</Show>
            <Show when={typeof e().fetchedAtMs === "number"}>
              <span>
                · {new Intl.DateTimeFormat(language.intl(), { timeStyle: "short" }).format(e().fetchedAtMs!)}
              </span>
            </Show>
            <Show when={e().cached}>
              <span>({language.t("settings.quotas.cached")})</span>
            </Show>
            <Badge variant={e().capability === "live_api" ? "accent" : "subtle"}>
              {e().capability === "live_api"
                ? language.t("settings.quotas.capability.live")
                : language.t("settings.quotas.capability.headers")}
            </Badge>
            <Badge variant={e().status === "error" ? "neutral" : "subtle"}>
              {language.t(statusLabelKey(e().status))}
            </Badge>
          </div>

          <Show when={e().error}>
            {(err) => (
              <div class="rounded-md border border-v2-border-border-base bg-v2-background-bg-base px-3 py-2 text-12-regular text-v2-state-fg-danger">
                {err()}
              </div>
            )}
          </Show>

          <Show when={e().warnings && e().warnings!.length > 0}>
            <div class="flex flex-col gap-1 rounded-md border border-v2-border-border-base bg-v2-background-bg-base px-3 py-2 text-11-regular text-v2-text-text-muted">
              <For each={e().warnings}>{(w) => <span>{w}</span>}</For>
            </div>
          </Show>

          <Show when={e().balances && e().balances!.length > 0}>
            <div class="flex flex-wrap gap-2">
              <For each={e().balances}>
                {(bal) => (
                  <div class="flex flex-wrap items-center gap-2 rounded-md border border-v2-border-border-base bg-v2-background-bg-base px-3 py-1.5 text-12-regular">
                    <span class="text-v2-text-text-muted">{language.t("settings.quotas.balance")}</span>
                    <Show when={typeof bal.remaining === "number"}>
                      <strong class="font-medium text-v2-text-text-base">
                        {formatCurrencyValue(bal.remaining!, bal.currency, language.intl())}
                      </strong>
                    </Show>
                    <Show when={typeof bal.total === "number"}>
                      <span class="text-11-regular text-v2-text-text-muted">
                        ({language.t("settings.quotas.balanceTotal")}:{" "}
                        {formatCurrencyValue(bal.total!, bal.currency, language.intl())})
                      </span>
                    </Show>
                    <Show when={typeof bal.toppedUp === "number" || typeof bal.granted === "number"}>
                      <span class="text-11-regular text-v2-text-text-muted">
                        {[
                          typeof bal.toppedUp === "number"
                            ? `${language.t("settings.quotas.toppedUp")} ${formatCurrencyValue(bal.toppedUp, bal.currency, language.intl())}`
                            : undefined,
                          typeof bal.granted === "number"
                            ? `${language.t("settings.quotas.granted")} ${formatCurrencyValue(bal.granted, bal.currency, language.intl())}`
                            : undefined,
                        ]
                          .filter(Boolean)
                          .join(" · ")}
                      </span>
                    </Show>
                  </div>
                )}
              </For>
            </div>
          </Show>

          <Show when={e().spendSummary}>
            {(spend) => (
              <div class="flex flex-wrap gap-3 text-11-regular text-v2-text-text-muted">
                <Show when={typeof spend().daily === "number"}>
                  <span>
                    {language.t("settings.quotas.apiSpend.daily")}:{" "}
                    <strong class="text-v2-text-text-base">
                      {formatCurrencyValue(spend().daily!, spend().currency, language.intl())}
                    </strong>
                  </span>
                </Show>
                <Show when={typeof spend().weekly === "number"}>
                  <span>
                    {language.t("settings.quotas.apiSpend.weekly")}:{" "}
                    <strong class="text-v2-text-text-base">
                      {formatCurrencyValue(spend().weekly!, spend().currency, language.intl())}
                    </strong>
                  </span>
                </Show>
                <Show when={typeof spend().monthly === "number"}>
                  <span>
                    {language.t("settings.quotas.apiSpend.monthly")}:{" "}
                    <strong class="text-v2-text-text-base">
                      {formatCurrencyValue(spend().monthly!, spend().currency, language.intl())}
                    </strong>
                  </span>
                </Show>
                <Show when={typeof spend().total === "number"}>
                  <span>
                    {language.t("settings.quotas.apiSpend.total")}:{" "}
                    <strong class="text-v2-text-text-base">
                      {formatCurrencyValue(spend().total!, spend().currency, language.intl())}
                    </strong>
                  </span>
                </Show>
              </div>
            )}
          </Show>

          <Show when={e().windows && e().windows!.length > 0}>
            <div class="flex flex-col gap-2.5">
              <For each={e().windows}>{(w) => <WindowRow window={w} />}</For>
            </div>
          </Show>

          <Show when={e().providerID === "openrouter"}>
            <OpenRouterManagementSection
              configured={Boolean(e().hasManagementKey)}
              envManaged={props.envManaged || e().managementKeySource === "env"}
              saving={props.savingMgmt}
              onSave={props.onSaveMgmtKey}
              onClear={props.onClearMgmtKey}
            />
          </Show>

          <Show when={e().accounts && e().accounts!.length > 0}>
            <AntigravityAccountsSection
              accounts={e().accounts!}
              busyId={props.busyAccountId}
              onAction={props.onAccountAction}
            />
          </Show>

          <Show when={e().rateLimit}>{(rl) => <RateLimitSection rateLimit={rl()} />}</Show>

          {/* Per-key rotation rows: the credential axis, never the model axis. */}
          <Show when={e().keys && e().keys!.length > 0}>
            <ProviderKeysSection keys={e().keys!} />
          </Show>

          <Show when={e().localUsage}>{(usage) => <LocalUsageSection usage={usage()} />}</Show>

          <Show when={e().capabilityNote}>
            {(note) => <div class="text-11-regular text-v2-text-text-muted">{note()}</div>}
          </Show>
        </div>
      </Show>
    </div>
  )
}

export const SettingsQuotas: Component = () => {
  const language = useLanguage()
  const quotas = useQuotasData({ refetchInterval: SETTINGS_QUOTAS_POLL_MS })
  const [refreshing, setRefreshing] = createSignal(false)
  const [savingMgmt, setSavingMgmt] = createSignal(false)
  const [busyAccountId, setBusyAccountId] = createSignal<string | undefined>()
  const [envManaged, setEnvManaged] = createSignal(false)
  const [openProviders, setOpenProviders] = createSignal<Record<string, boolean>>({})
  const data = () => quotasData(quotas.query.data)
  const unavailable = () => quotasUnavailable(quotas.query.data)
  const providerIds = createMemo<string[]>(
    () => (data()?.providers ?? []).map((p) => p.providerID),
    [],
    {
      equals: (a, b) => a.length === b.length && a.every((v, i) => v === b[i]),
    },
  )
  const toggleProvider = (id: string) => {
    setOpenProviders((prev) => ({ ...prev, [id]: !prev[id] }))
  }

  const onRefresh = async () => {
    setRefreshing(true)
    try {
      await quotas.refreshLive()
    } catch (error) {
      showToast({
        title: language.t("common.requestFailed"),
        description: error instanceof Error ? error.message : String(error),
      })
    } finally {
      setRefreshing(false)
    }
  }

  const onAccountAction = async (id: string, action: "select" | "enable" | "disable") => {
    setBusyAccountId(id)
    setEnvManaged(false)
    try {
      await quotas.mutateAntigravityAccount(id, action)
    } catch (error) {
      showToast({
        title: language.t("common.requestFailed"),
        description: error instanceof Error ? error.message : String(error),
      })
    } finally {
      setBusyAccountId(undefined)
    }
  }

  const onSaveMgmt = async (key: string) => {
    setSavingMgmt(true)
    setEnvManaged(false)
    try {
      await quotas.saveOpenRouterManagementKey(key)
      showToast({
        variant: "success",
        icon: "circle-check",
        title: language.t("settings.quotas.openrouter.mgmtSaved"),
      })
    } catch (error) {
      // A key owned by the environment is a state, not a failure: say why in place.
      if (error instanceof QuotaRequestError && error.kind === "env_managed") {
        setEnvManaged(true)
      } else {
        showToast({
          title: language.t("common.requestFailed"),
          description: error instanceof Error ? error.message : String(error),
        })
      }
    } finally {
      setSavingMgmt(false)
    }
  }

  const onClearMgmt = async () => {
    setSavingMgmt(true)
    setEnvManaged(false)
    try {
      await quotas.clearOpenRouterManagementKey()
    } catch (error) {
      // The env var owns the key, so Clear cannot remove it: surface that as the explanation.
      if (error instanceof QuotaRequestError && error.kind === "env_managed") {
        setEnvManaged(true)
        return
      }
      showToast({
        title: language.t("common.requestFailed"),
        description: error instanceof Error ? error.message : String(error),
      })
    } finally {
      setSavingMgmt(false)
    }
  }

  return (
    <>
      <div class="settings-tab-header">
        <div class="settings-tab-header-row">
          <div class="flex flex-col gap-1">
            <h2 class="settings-tab-title">{language.t("settings.tab.quotas")}</h2>
            <span class="text-11-regular text-v2-text-text-muted">{language.t("settings.quotas.description")}</span>
          </div>
          <Button
            size="small"
            variant="neutral"
            icon="outline-rotate-clockwise"
            disabled={refreshing() || quotas.query.isPending || Boolean(unavailable())}
            onClick={() => void onRefresh()}
            data-testid="settings-quotas-refresh"
          >
            {refreshing() ? language.t("settings.quotas.refreshing") : language.t("settings.quotas.refresh")}
          </Button>
        </div>
      </div>

      <div class="settings-tab-body">
        <Show when={quotas.query.isPending}>
          <div class="flex min-h-[160px] items-center justify-center">
            <Loader />
          </div>
        </Show>

        <Show when={unavailable()}>
          {(state) => (
            <div
              role="status"
              data-testid="settings-quotas-unavailable"
              class="flex flex-col gap-1 rounded-md border border-v2-border-border-base bg-v2-background-bg-layer-01 px-3 py-2"
            >
              <div class="text-12-medium">{language.t("settings.quotas.unavailable.title")}</div>
              <div class="text-11-regular text-v2-text-text-muted">
                {language.t("settings.quotas.unavailable.description")}
              </div>
              <Show when={state().message}>
                {(message) => <div class="text-11-regular text-v2-text-text-faint">{message()}</div>}
              </Show>
            </div>
          )}
        </Show>

        <Show when={quotas.query.isError}>
          <div class="flex flex-col items-start gap-2 rounded-md border border-v2-border-border-base bg-v2-background-bg-layer-01 p-4">
            <span class="text-13-regular text-v2-state-fg-danger">
              {quotas.query.error instanceof Error
                ? quotas.query.error.message
                : language.t("settings.quotas.loadError")}
            </span>
            <Button size="small" variant="neutral" onClick={() => void quotas.query.refetch()}>
              {language.t("usage.retry")}
            </Button>
          </div>
        </Show>

        <Show when={data()}>
          {(snapshot) => (
            <div class="flex flex-col gap-4">
              <Collapsible variant="ghost" defaultOpen>
                <Collapsible.Trigger>
                  <span>{language.t("settings.quotas.usage.title")}</span>
                  <Show when={quotasRefreshing(quotas.query.data)}>
                    <span data-testid="settings-quotas-refreshing" class="flex items-center gap-1.5 text-11-regular text-v2-text-text-faint">
                      <Icon name="outline-rotate-clockwise" size="small" />
                      {language.t("settings.quotas.refreshing")}
                    </span>
                  </Show>
                  <Collapsible.Arrow />
                </Collapsible.Trigger>
                <Collapsible.Content>
                  <div class="flex flex-col gap-3 pt-2">
                    {/*
                      One row per provider; the compact row owns its own numbers
                      and expands to the key rotation list. Totals live once, in
                      this header row.
                    */}
                    <div class="flex flex-wrap items-center gap-x-3 text-11-regular text-v2-text-text-muted">
                      <span class="flex-1">
                        {language.t("settings.quotas.usage.providers", {
                          count: snapshot().summary.liveQuotaCount,
                          total: snapshot().summary.providerCount,
                        })}
                      </span>
                      <span class="w-24 text-end">
                        {language.t("settings.quotas.usage.spend", {
                          amount: formatCurrencyValue(snapshot().summary.spend24hUsd, "USD", language.intl()),
                        })}
                      </span>
                    </div>

                    <Show
                      when={snapshot().providers.length > 0}
                      fallback={
                        <div class="rounded-md border border-v2-border-border-base bg-v2-background-bg-layer-01 p-4 text-13-regular text-v2-text-text-muted">
                          {language.t("settings.quotas.empty")}
                        </div>
                      }
                    >
                      <div class="flex flex-col gap-1.5">
                        <For each={providerIds()}>
                          {(providerID) => {
                            const current = () =>
                              snapshot().providers.find((item) => item.providerID === providerID)
                            return (
                              <Show when={current()}>
                                {(entry) => (
                                  <ProviderQuotaRow
                                    entry={entry()}
                                    open={Boolean(openProviders()[providerID])}
                                    onToggle={() => toggleProvider(providerID)}
                                    envManaged={envManaged()}
                                    busyAccountId={busyAccountId()}
                                    savingMgmt={savingMgmt()}
                                    onAccountAction={(id, action) => void onAccountAction(id, action)}
                                    onSaveMgmtKey={onSaveMgmt}
                                    onClearMgmtKey={onClearMgmt}
                                  />
                                )}
                              </Show>
                            )
                          }}
                        </For>
                      </div>
                    </Show>
                  </div>
                </Collapsible.Content>
              </Collapsible>
            </div>
          )}
        </Show>
      </div>
    </>
  )
}
