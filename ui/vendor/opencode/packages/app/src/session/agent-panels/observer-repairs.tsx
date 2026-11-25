import { Icon } from "@opencode/ui/icon"
import { For, Show, createMemo, createSignal, type Accessor } from "solid-js"
import { useLanguage } from "@/runtime/i18n/language"
import { useServer } from "@/runtime/server/current"
import { useOpenSessionRoute } from "@/session/session-identity-header"
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
  rootID?: string
  live: Accessor<ObserverRepair[]>
  escalation: Accessor<{ reason: string; budget?: number } | undefined>
  inline?: boolean
}) {
  const language = useLanguage()
  const server = useServer()
  const open = useOpenSessionRoute()
  const targetSessionID = () => props.rootID || props.sessionID
  const history = useObserverRepairsQuery(targetSessionID)
  const state = useObserverStateQuery(targetSessionID)

  const initialCollapsed = () => {
    if (typeof localStorage !== "undefined") {
      const stored = localStorage.getItem("opencode.session.observer-repairs.v2.collapsed")
      if (stored !== null) return stored === "true"
    }
    return true
  }

  const [collapsed, setCollapsed] = createSignal(initialCollapsed())
  const toggleCollapsed = () => {
    setCollapsed((val) => {
      const next = !val
      if (typeof localStorage !== "undefined") {
        localStorage.setItem("opencode.session.observer-repairs.v2.collapsed", String(next))
      }
      return next
    })
  }

  const rowIdentity = (rows: ObserverRepair[]) => rows.map(identity).join("\u0001")

  const rows = createMemo(() => {
    const fetched = history.data?.available ? history.data.value : []
    const seen = new Set(fetched.map(identity))
    const live = props.live().filter((row) => !seen.has(identity(row)))

    return [...live, ...fetched].slice(0, 20)
  }, [], { equals: (a, b) => rowIdentity(a) === rowIdentity(b) })

  const budget = createMemo(() => (state.data?.available ? state.data.value.budget : undefined))

  const exhausted = createMemo(() => {
    if (props.escalation()) return true
    return budget()?.exhausted === true
  })

  // What the observer reported, else the engine's own repair counter.
  const reason = createMemo(() => {
    const escalation = props.escalation()
    if (escalation?.reason) return escalation.reason
    const b = budget()
    if (!b) return undefined

    return language.t("session.agentPanels.observer.budget.usage", {
      used: String(b.used),
      limit: String(b.limit),
    })
  })

  const targetLabel = (target: string) => {
    const info = server.ctx.data.session.get(target)
    return info?.title?.trim() || target
  }

  const isSessionTarget = (target: string | undefined) =>
    Boolean(target && (target.startsWith("ses_") || server.ctx.data.session.get(target)))

  let repairListRef: HTMLOListElement | undefined
  const stopIfScrollable = (e: Event) => {
    if (repairListRef && repairListRef.scrollHeight > repairListRef.clientHeight) {
      e.stopPropagation()
    }
  }

  return (
    <Show when={rows().length > 0 || exhausted() || props.escalation()}>
      <div
        data-component="session-observer-repairs"
        class="relative border-b border-v2-border-border-base bg-v2-background-bg-base"
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
          class="flex w-full items-center gap-2 px-4 py-1.5 text-left text-11-regular text-v2-text-text-muted transition-colors hover:bg-v2-overlay-simple-overlay-hover md:px-6"
          aria-expanded={!collapsed()}
          aria-label={language.t(
            collapsed() ? "session.agentPanels.observer.expand" : "session.agentPanels.observer.collapse",
          )}
          onClick={toggleCollapsed}
        >
          <Icon name="outline-brain" size="small" class="shrink-0 text-v2-icon-icon-faint" />
          <span class="shrink-0 font-medium text-v2-text-text-base">
            {language.t("session.agentPanels.observer.title")}
          </span>
          <span class="shrink-0 rounded-full bg-v2-surface-surface-muted px-2 py-0.5 text-[11px] tabular-nums text-v2-text-text-muted">
            {language.t("session.agentPanels.observer.count", { count: String(rows().length) })}
          </span>
          <Show when={budget()}>
            {(b) => (
              <span
                class="hidden shrink-0 rounded-full bg-v2-surface-surface-muted px-2 py-0.5 text-[11px] tabular-nums sm:inline"
                classList={{
                  "text-v2-state-fg-warning": b().exhausted || b().used >= b().limit,
                  "text-v2-text-text-faint": !b().exhausted && b().used < b().limit,
                }}
              >
                {language.t("session.agentPanels.observer.budget.usage", {
                  used: String(b().used),
                  limit: String(b().limit),
                })}
              </span>
            )}
          </Show>
          <Show when={collapsed() && rows()[0]}>
            {(latest) => (
              <span class="hidden min-w-0 flex-1 items-center gap-1.5 truncate text-11-regular text-v2-text-text-faint md:inline-flex">
                <span>·</span>
                <span class="shrink-0 font-mono text-[11px] text-v2-text-text-muted">{latest().kind}</span>
                <Show when={latest().reason}>
                  <span class="min-w-0 truncate" dir="auto">
                    {latest().reason}
                  </span>
                </Show>
              </span>
            )}
          </Show>
          <span class="flex-1" />
          <Icon name={collapsed() ? "outline-chevron-right" : "chevron-down"} size="small" class="shrink-0" />
        </button>
        <Show when={!collapsed()}>
          <div
            class="border-b border-v2-border-border-base bg-v2-background-bg-base"
            classList={{
              "absolute inset-x-0 top-full z-40 shadow-[0_12px_32px_rgba(0,0,0,0.35)]": !props.inline,
            }}
            onWheel={stopIfScrollable}
            onTouchMove={stopIfScrollable}
          >
            <ol
              ref={repairListRef}
              data-scrollable={props.inline ? undefined : ""}
              class="py-1"
              classList={{
                "max-h-[min(240px,45vh)] overflow-y-auto": !props.inline,
              }}
              data-testid="session-observer-repair-rows"
            >
              <For each={rows()}>
                {(row) => (
                  <li class="flex items-center gap-2.5 px-4 py-1 text-12-regular transition-colors hover:bg-v2-overlay-simple-overlay-hover/50 md:px-6">
                    <span
                      class="shrink-0 rounded px-1.5 py-0.5 font-mono text-[11px] font-medium"
                      classList={{
                        "bg-v2-surface-surface-muted text-v2-state-fg-success":
                          row.kind === "extend_budget" || row.kind === "spawn_helper",
                        "bg-v2-surface-surface-muted text-v2-state-fg-warning":
                          row.kind === "nudge" || row.kind === "re_prompt",
                        "bg-v2-surface-surface-muted text-v2-state-fg-danger":
                          row.kind === "switch_model" || row.kind === "escalate",
                        "bg-v2-surface-surface-muted text-v2-text-text-base":
                          row.kind !== "extend_budget" &&
                          row.kind !== "spawn_helper" &&
                          row.kind !== "nudge" &&
                          row.kind !== "re_prompt" &&
                          row.kind !== "switch_model" &&
                          row.kind !== "escalate",
                      }}
                    >
                      {row.kind}
                    </span>
                    <span class="min-w-0 flex-1 truncate text-v2-text-text-muted" dir="auto" title={row.reason}>
                      {row.reason || language.t("session.agentPanels.observer.noReason")}
                    </span>
                    <Show when={row.target}>
                      {(target) => (
                        <Show
                          when={isSessionTarget(target()) && target() !== props.sessionID}
                          fallback={
                            <span
                              class="max-w-[180px] shrink-0 truncate font-mono text-11-regular text-v2-text-text-faint"
                              dir="auto"
                            >
                              {targetLabel(target())}
                            </span>
                          }
                        >
                          <button
                            type="button"
                            class="max-w-[180px] shrink-0 truncate rounded bg-v2-surface-surface-muted/60 px-1.5 py-0.5 font-mono text-11-regular text-v2-text-text-muted underline-offset-2 transition-colors hover:bg-v2-surface-surface-muted hover:text-v2-text-text-base hover:underline"
                            title={targetLabel(target())}
                            onClick={() => {
                              setCollapsed(true)
                              open(props.sessionID, target())
                            }}
                          >
                            ↗ {targetLabel(target())}
                          </button>
                        </Show>
                      )}
                    </Show>
                    <span class="shrink-0 rounded bg-v2-surface-surface-muted/60 px-1.5 py-0.5 text-11-regular tabular-nums text-v2-text-text-faint">
                      {language.t("session.agentPanels.observer.attempt", { count: String(row.attempt) })}
                    </span>
                  </li>
                )}
              </For>
            </ol>
          </div>
        </Show>
      </div>
    </Show>
  )
}

function identity(row: ObserverRepair) {
  return `${row.kind}\u0000${row.target}\u0000${row.attempt}\u0000${row.reason}`
}
