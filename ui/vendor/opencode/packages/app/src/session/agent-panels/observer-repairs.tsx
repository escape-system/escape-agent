import { Icon } from "@opencode/ui/icon"
import { For, Show, createMemo, createSignal, type Accessor } from "solid-js"
import { useLanguage } from "@/runtime/i18n/language"
import { useObserverRepairsQuery, useObserverStateQuery } from "./agent-queries"
import type { ObserverRepair } from "./agent-wire"

/**
 * Observer repair audit for the session tree (`docs/13-ui.md`).
 *
 * The fetched rows are the durable audit; live `observer.repaired` events are
 * merged on top, deduplicated by kind/target/attempt because the event carries
 * no row id. `observer.escalated` and the engine's budget turn into the banner:
 * the observer is out of repairs for this tree.
 */
export function ObserverRepairPanel(props: {
  sessionID: string
  live: Accessor<ObserverRepair[]>
  escalation: Accessor<{ reason: string; budget?: number } | undefined>
}) {
  const language = useLanguage()
  const history = useObserverRepairsQuery(() => props.sessionID)
  const state = useObserverStateQuery(() => props.sessionID)
  const [collapsed, setCollapsed] = createSignal(false)

  const rows = createMemo(() => {
    const fetched = history.data?.available ? history.data.value : []
    const seen = new Set(fetched.map(identity))
    const live = props.live().filter((row) => !seen.has(identity(row)))

    return [...live, ...fetched].slice(0, 20)
  })

  const exhausted = createMemo(() => {
    if (props.escalation()) return true
    const budget = state.data?.available ? state.data.value.budget : undefined

    return budget?.exhausted === true
  })

  // What the observer reported, else the engine's own repair counter.
  const reason = createMemo(() => {
    const escalation = props.escalation()
    if (escalation?.reason) return escalation.reason
    const budget = state.data?.available ? state.data.value.budget : undefined
    if (!budget) return undefined

    return language.t("session.agentPanels.observer.budget.usage", {
      used: String(budget.used),
      limit: String(budget.limit),
    })
  })

  return (
    <Show when={rows().length > 0 || exhausted() || props.escalation()}>
      <div
        data-component="session-observer-repairs"
        class="border-b border-v2-border-border-base bg-v2-background-bg-base"
      >
        <Show when={exhausted()}>
          <div
            role="alert"
            data-testid="session-observer-budget"
            class="flex items-start gap-2 border-b border-v2-border-border-base bg-v2-background-bg-layer-01 px-4 py-1.5 md:px-6"
          >
            <Icon name="circle-exclamation" size="small" class="mt-0.5 shrink-0 text-v2-state-fg-warning" />
            <div class="min-w-0">
              <div class="text-12-medium text-v2-text-text-base">
                {language.t("session.agentPanels.observer.budget.title")}
              </div>
              <div class="text-11-regular text-v2-text-text-muted">
                {language.t("session.agentPanels.observer.budget.description")}
              </div>
              <Show when={reason()}>
                {(reason) => (
                  <div class="truncate text-11-regular text-v2-text-text-faint" dir="auto">
                    {reason()}
                  </div>
                )}
              </Show>
            </div>
          </div>
        </Show>
        <button
          type="button"
          class="flex w-full items-center gap-2 px-4 py-1.5 text-left text-11-regular text-v2-text-text-muted hover:bg-v2-overlay-simple-overlay-hover md:px-6"
          aria-expanded={!collapsed()}
          aria-label={language.t(
            collapsed() ? "session.agentPanels.observer.expand" : "session.agentPanels.observer.collapse",
          )}
          onClick={() => setCollapsed((value) => !value)}
        >
          <Icon name="outline-brain" size="small" class="shrink-0 text-v2-icon-icon-faint" />
          <span class="shrink-0">{language.t("session.agentPanels.observer.title")}</span>
          <span class="shrink-0 tabular-nums text-v2-text-text-faint">
            {language.t("session.agentPanels.observer.count", { count: String(rows().length) })}
          </span>
          <span class="flex-1" />
          <Icon name={collapsed() ? "outline-chevron-right" : "chevron-down"} size="small" class="shrink-0" />
        </button>
        <Show when={!collapsed()}>
          <ol class="pb-1" data-testid="session-observer-repair-rows">
            <For each={rows()}>
              {(row) => (
                <li class="flex items-baseline gap-2 px-4 py-0.5 text-12-regular md:px-6">
                  <span class="w-28 shrink-0 truncate font-mono text-11-regular text-v2-text-text-muted">
                    {row.kind}
                  </span>
                  <span class="min-w-0 flex-1">
                    <span class="me-2 text-v2-text-text-muted" dir="auto">
                      {row.reason || language.t("session.agentPanels.observer.noReason")}
                    </span>
                    <Show when={row.target}>
                      <span class="font-mono text-11-regular text-v2-text-text-faint" dir="auto">
                        {row.target}
                      </span>
                    </Show>
                  </span>
                  <span class="shrink-0 text-11-regular tabular-nums text-v2-text-text-faint">
                    {language.t("session.agentPanels.observer.attempt", { count: String(row.attempt) })}
                  </span>
                </li>
              )}
            </For>
          </ol>
        </Show>
      </div>
    </Show>
  )
}

function identity(row: ObserverRepair) {
  return `${row.kind}\u0000${row.target}\u0000${row.attempt}\u0000${row.reason}`
}
