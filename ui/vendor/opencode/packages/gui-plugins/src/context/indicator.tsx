import { Show, createMemo, type ComponentProps, type JSX } from "solid-js"
import { ProgressCircle } from "@opencode/ui/progress-circle"
import { Button } from "@opencode/ui/button"
import { Tooltip } from "@opencode/ui/tooltip"
import { useI18n } from "@opencode/ui/context/i18n"
import { lastAssistantWithUsage } from "@opencode/client/solid"
import { usePlugin, type MountedSession } from "@opencode/plugin/gui"
import { catalogModel, syncCatalog } from "./catalog"

function ContextTooltipRow(props: { name: JSX.Element; value: JSX.Element }) {
  return (
    <div class="flex min-w-0 items-center gap-4">
      <span class="shrink-0 text-v2-text-text-muted">{props.name}</span>
      <span class="ml-auto min-w-0 truncate text-right text-v2-text-text-base">{props.value}</span>
    </div>
  )
}

export function SessionContextUsage(props: {
  session: MountedSession
  variant?: "button" | "indicator"
  placement?: ComponentProps<typeof Tooltip>["placement"]
}) {
  const ctx = usePlugin()
  const layout = ctx.layout
  const i18n = useI18n()
  syncCatalog(() => props.session)

  const variant = createMemo(() => props.variant ?? "button")

  const messages = createMemo(() =>
    props.session.id ? props.session.server.data.session.message.list(props.session.id) : [],
  )

  const usd = createMemo(
    () =>
      new Intl.NumberFormat(i18n.locale(), {
        style: "currency",
        currency: "USD",
      }),
  )

  const info = createMemo(() =>
    props.session.id ? props.session.server.data.session.get(props.session.id) : undefined,
  )

  // Per-step usage from `session.usage.updated` (live, resets on compaction). The
  // denominator is the context ceiling, `window - max(15% of window, 16000)`: 100%
  // is the hard limit, not the threshold. Compaction triggers below it by the
  // buffer, at `ceiling - buffer` (unset `compaction.buffer` = `max(8_000, window/10)`).
  // NOTE: `usage` below divides by the ceiling, not by the trigger, so the circle
  // reaches 100% at the hard limit; compaction actually fires earlier, at ~88% of
  // this denominator for a 200k window (150k of 170k). Also `usageLabel` still
  // prints `limit`, the raw window, beside that percentage. Recomputing either is a
  // separate change, deliberately not made here.
  const context = createMemo(() => {
    const step = props.session.id ? props.session.server.data.session.lastStep(props.session.id) : undefined
    const message = step ? undefined : lastAssistantWithUsage(messages(), info()?.revert?.messageID)
    const tokens = step?.tokens ?? message?.tokens

    if (!tokens) return

    const ref =
      step === undefined
        ? message!.model
        : (info()?.model ?? messages().findLast((item) => item.type === "assistant")?.model)
    const limit = ref ? catalogModel(props.session, ref)?.model?.limit.context : undefined

    const total = tokens.input + tokens.output + tokens.reasoning + tokens.cache.read + tokens.cache.write
    // Reserve room for the next output/protocol overhead, not the compaction buffer:
    // `ceiling = window - max(15% of window, 16000)` matches `tokens::ceiling`. The
    // buffer that lowers the compaction trigger is separate: `max(8_000, window/10)`
    // when `compaction.buffer` is unset, so the trigger is `ceiling - buffer`.
    const ceiling = limit ? Math.max(1, limit - Math.max(limit * 0.15, 16000)) : undefined

    return {
      total,
      limit,
      usage: ceiling ? Math.min(100, Math.round((total / ceiling) * 100)) : null,
    }
  })

  const tokens = createMemo(() =>
    new Intl.NumberFormat(i18n.locale(), { notation: "compact", maximumFractionDigits: 0 })
      .format(context()?.total ?? 0)
      .toLocaleLowerCase(i18n.locale()),
  )

  // A root session's cost includes its subagent sessions, as in the TUI.
  const spent = createMemo(() => (props.session.id ? props.session.server.data.session.cost(props.session.id) : 0))

  const cost = createMemo(() => usd().format(spent()))

  // Spend anywhere in the family keeps the cost visible, even when this session's model is free.
  const showCost = createMemo(() => {
    if (spent() > 0) return true
    const ref = info()?.model ?? messages().findLast((item) => item.type === "assistant")?.model
    const model = ref ? catalogModel(props.session, ref)?.model : undefined

    if (!model) return true

    return model.cost.some((cost) => cost.input > 0 || cost.output > 0 || cost.cache.read > 0 || cost.cache.write > 0)
  })

  const openContext = () => {
    if (!props.session.id) return
    layout.toggle(`${ctx.id}:main`, props.session)
  }

  const circle = () => (
    <div class="flex items-center justify-center">
      <ProgressCircle
        appearance="indicator"
        size={16}
        strokeWidth={2}
        percentage={context()?.usage ?? 0}
        style={{
          "--progress-circle-background": "var(--v2-background-bg-layer-04)",
          "--progress-circle-background-overlay": "var(--v2-overlay-simple-overlay-pressed, transparent)",
          "--progress-circle-progress": "var(--v2-icon-icon-base, var(--icon-base))",
        }}
      />
    </div>
  )

  const compactCircle = () => (
    <div class="flex items-center justify-center">
      <ProgressCircle appearance="compact" size={16} percentage={context()?.usage ?? 0} />
    </div>
  )

  const usageLabel = createMemo(() => {
    const c = context()
    if (!c) return "—"
    if (c.usage === null) return "—"
    if (!c.limit) return `${c.usage}%`
    return `${c.usage}% / ${c.limit.toLocaleString(i18n.locale())}`
  })

  const tooltipValue = () => (
    <div class="flex w-[120px] flex-col gap-2">
      <Show when={showCost()}>
        <ContextTooltipRow name={ctx.t("usage.cost")} value={cost()} />
      </Show>
      <ContextTooltipRow name={ctx.t("usage.usage")} value={usageLabel()} />
      <ContextTooltipRow name={ctx.t("usage.tokens")} value={context()?.total.toLocaleString(i18n.locale()) ?? "0"} />
    </div>
  )

  return (
    <Show when={props.session.id}>
      <Tooltip
        value={variant() === "indicator" ? tooltipValue() : ctx.t("usage.toggle")}
        placement={props.placement ?? "top"}
      >
        <Show
          when={variant() === "indicator"}
          fallback={
            <Button
              type="button"
              variant="ghost-muted"
              class="group shrink-0"
              title={context()?.limit ? `context window ${context()!.limit.toLocaleString(i18n.locale())}` : undefined}
              style={{ padding: "0 6px", color: "var(--v2-text-text-faint)" }}
              onClick={openContext}
              aria-expanded={layout.state(`${ctx.id}:main`, props.session) === "visible"}
              aria-label={ctx.t("usage.toggle")}
            >
              <span class="flex items-center gap-2 whitespace-nowrap group-active:text-v2-text-text-muted">
                {compactCircle()}
                <span>{tokens()}</span>
                <Show when={showCost()}>
                  <span aria-hidden="true" class="flex w-1.5 shrink-0 items-center justify-center">
                    ·
                  </span>
                  <span>{cost()}</span>
                </Show>
              </span>
            </Button>
          }
        >
          {circle()}
        </Show>
      </Tooltip>
    </Show>
  )
}
