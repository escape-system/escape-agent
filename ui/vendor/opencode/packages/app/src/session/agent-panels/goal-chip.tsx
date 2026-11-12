import { Icon } from "@opencode/ui/icon"
import { Show } from "solid-js"
import { useLanguage } from "@/runtime/i18n/language"
import { useServer } from "@/runtime/server/current"
import { useSettingsSurface } from "@/settings/surface"
import { createAgentEvents } from "./agent-events"
import { useSessionGoalQuery } from "./agent-queries"
import type { Goal } from "./agent-wire"

/**
 * The composer chip for the session tree's goal (`docs/13-ui.md`):
 * `objective · 2/5 criteria`. Clicking it opens the goals page; `goal.updated`
 * replaces the fetched goal without a refetch.
 */
export function SessionGoalChip(props: { sessionID: string }) {
  const language = useLanguage()
  const server = useServer()
  const surface = useSettingsSurface()
  const query = useSessionGoalQuery(() => props.sessionID)
  const events = createAgentEvents({ sessionID: () => props.sessionID, root: () => props.sessionID })

  const goal = () => events.goal() ?? (query.data?.available ? query.data.value : undefined)

  // Prefer the server's own `done`/`total`: only the observer may accept a
  // criterion, and evidence ids are opaque handles that are never attributed
  // to individual criteria (`docs/08-goals-observer.md` §7). The evidence
  // length is a display fallback for rows that predate those fields.
  const progress = (current: Goal) => {
    const total = current.total ?? current.criteria.length
    const done = current.done ?? Math.min(current.evidence.length, total)
    return { done: String(done), total: String(total) }
  }

  return (
    <Show when={goal()}>
      {(current) => (
        <button
          type="button"
          data-component="session-goal-chip"
          data-status={current().status}
          class="pointer-events-auto flex h-6 max-w-[min(320px,50vw)] items-center gap-1.5 rounded-full border border-v2-border-border-base bg-v2-background-bg-layer-01 px-2.5 text-11-regular text-v2-text-text-muted hover:bg-v2-background-bg-layer-02"
          title={current().objective}
          aria-label={language.t("session.goal.open", { objective: current().objective })}
          onClick={() => surface.replace({ type: "root", tab: "goals", server: server.key })}
        >
          <Icon name="status-active" size="small" class="shrink-0 text-v2-icon-icon-faint" />
          <span class="min-w-0 truncate" dir="auto">
            {current().objective}
          </span>
          <Show when={current().criteria.length > 0}>
            <span class="shrink-0 tabular-nums text-v2-text-text-faint">
              {language.t("session.goal.criteria", progress(current()))}
            </span>
          </Show>
        </button>
      )}
    </Show>
  )
}
