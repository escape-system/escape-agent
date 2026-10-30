import { Icon } from "@opencode/ui/icon"
import { Show, createMemo } from "solid-js"
import { useLanguage } from "@/runtime/i18n/language"
import { createAgentEvents } from "./agent-events"
import { useLaneSnapshotQuery } from "./agent-queries"

/**
 * The lane pill (`docs/13-ui.md`): the lane this session's build command is
 * waiting in, with the lane's queue depth. With no lane service attached the
 * server answers `503`, and the pill says so inline instead of showing a
 * healthy empty queue.
 */
export function SessionLanePill(props: { sessionID: string }) {
  const language = useLanguage()
  const snapshot = useLaneSnapshotQuery()
  const events = createAgentEvents({ sessionID: () => props.sessionID, root: () => props.sessionID })

  const ticket = createMemo(() => events.activeTickets()[0])
  // `cargo:/path/to/worktree` is shown by the resource the queue serialises.
  const laneName = createMemo(() => ticket()?.lane.split(":")[0] ?? "")
  const queueDepth = createMemo(() => {
    const current = ticket()
    if (!current || !snapshot.data?.available) return undefined

    return snapshot.data.value.find((item) => item.lane === current.lane)?.queued
  })
  const unavailable = createMemo(() => (snapshot.data && !snapshot.data.available ? snapshot.data.reason : undefined))

  return (
    <Show
      when={ticket()}
      fallback={
        <Show when={unavailable()}>
          {(reason) => (
            <span
              data-component="session-lane-pill"
              data-state="unavailable"
              class="pointer-events-auto flex h-6 items-center gap-1.5 rounded-full border border-v2-border-border-base bg-v2-background-bg-layer-01 px-2.5 text-11-regular text-v2-text-text-faint"
              title={reason()}
            >
              <Icon name="outline-worktree" size="small" class="shrink-0 text-v2-icon-icon-faint" />
              <span class="shrink-0">{language.t("session.agentPanels.lane.unavailable")}</span>
            </span>
          )}
        </Show>
      }
    >
      {(current) => (
        <span
          data-component="session-lane-pill"
          data-state={current().state}
          class="pointer-events-auto flex h-6 items-center gap-1.5 rounded-full border border-v2-border-border-base bg-v2-background-bg-layer-01 px-2.5 text-11-regular text-v2-text-text-muted"
          title={current().command}
        >
          <Icon name="outline-worktree" size="small" class="shrink-0 text-v2-icon-icon-faint" />
          <span class="shrink-0 font-mono">{laneName()}</span>
          <span class="shrink-0">{language.t(`session.agentPanels.lane.state.${current().state}`)}</span>
          <Show when={queueDepth()}>
            {(count) => (
              <span class="shrink-0 tabular-nums">
                · {language.t("session.agentPanels.lane.queue", { count: String(count()) })}
              </span>
            )}
          </Show>
        </span>
      )}
    </Show>
  )
}
