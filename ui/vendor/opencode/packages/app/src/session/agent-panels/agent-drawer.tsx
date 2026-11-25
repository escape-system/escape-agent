import { Icon } from "@opencode/ui/icon"
import { Show, Suspense, createEffect, createSignal, lazy, type JSX } from "solid-js"
import { useLanguage } from "@/runtime/i18n/language"
import { createAgentEvents } from "./agent-events"
import { useSubagentTreeQuery } from "./agent-queries"
import { ObserverRepairPanel } from "./observer-repairs"
import { SubagentTreePanel } from "./subagent-tree"
import { subagentCount, subagentCountIn } from "./subagent-tree-state"

const MobilePanelDrawer = lazy(async () => {
  const { MobilePanelDrawer } = await import("@/shell/mobile-panel-drawer")

  return { default: MobilePanelDrawer }
})

/**
 * The mobile entry point for the agent panels (`docs/13-ui.md`).
 *
 * The desktop header owns `SessionAgentPanels`, but that header is hidden on
 * phones. This component is the location-bar replacement: one 44px trigger that
 * opens a bottom sheet holding the same tree and observer rows. It keeps the
 * header's `resolvedRoot` latch so an observer refetch cannot flip the query key
 * and blank the sheet, and it hides itself when the server has nothing to show.
 */
export function SessionAgentPanelsMobile(props: { sessionID: string }): JSX.Element {
  const language = useLanguage()
  const tree = useSubagentTreeQuery(() => props.sessionID)
  const [resolvedRoot, setResolvedRoot] = createSignal<string>()
  const [open, setOpen] = createSignal(false)
  let trigger: HTMLButtonElement | undefined

  // Same latch as `agent-panels.tsx`: resolved once, then kept.
  createEffect(() => {
    const next = tree.data?.available ? tree.data.value.root : undefined

    if (next) setResolvedRoot(next)
  })
  const root = () => resolvedRoot() ?? props.sessionID
  const events = createAgentEvents({ sessionID: () => props.sessionID, root })

  const close = () => {
    setOpen(false)
    // The `<Show>` below unmounts the sheet in the same tick, so corvu's
    // `finalFocusEl` restore never runs for this sheet. Hand focus back to the
    // opener ourselves so keyboard and switch users are not dropped to <body>.
    queueMicrotask(() => {
      if (trigger && document.activeElement !== trigger) trigger.focus()
    })
  }

  const nodes = () => (tree.data?.available ? tree.data.value.nodes : [])
  const rootID = () => (tree.data?.available ? tree.data.value.root : undefined)
  // Same total as the desktop pill: every node except the family root, which is the session itself.
  const total = () => subagentCount(nodes(), rootID())
  const busy = () => subagentCountIn(nodes(), ["busy"], rootID())
  const available = () => tree.data?.available === true
  // A lone session is the routed one; the trigger would open an empty sheet.
  const visible = () => available() && (total() > 0 || events.repairs().length > 0 || !!events.escalation())

  return (
    <Show when={visible()}>
      <button
        ref={(element: HTMLButtonElement) => {
          trigger = element
        }}
        type="button"
        data-component="session-agent-panels-mobile-trigger"
        class="relative grid size-11 shrink-0 place-items-center rounded-lg text-v2-icon-icon-faint transition-colors hover:bg-v2-overlay-simple-overlay-hover hover:text-v2-text-text-base aria-expanded:bg-v2-overlay-simple-overlay-active"
        aria-label={language.t("session.agentPanels.openMobile")}
        aria-haspopup="dialog"
        aria-expanded={open()}
        onClick={() => setOpen(true)}
      >
        <Icon name="branch-out" />
        <Show when={busy() > 0}>
          <span
            data-testid="session-agent-panels-mobile-busy"
            class="pointer-events-none absolute end-0.5 top-0.5 min-w-4 rounded-full bg-v2-background-bg-layer-02 px-1 text-center text-[10px] leading-4 tabular-nums text-v2-state-fg-success"
          >
            {busy()}
          </span>
        </Show>
      </button>
      <Show when={open()}>
        <Suspense>
          <MobilePanelDrawer
            title={language.t("session.agentPanels.mobile.title")}
            open={open()}
            onOpenChange={(next) => {
              if (next) setOpen(true)
              else close()
            }}
            returnFocus={() => trigger}
          >
            <div
              data-component="session-agent-panels-mobile-content"
              class="flex min-h-0 flex-col"
              style={{
                "padding-bottom": "max(8px, var(--safe-area-inset-bottom, env(safe-area-inset-bottom, 0px)))",
              }}
            >
              <SubagentTreePanel sessionID={props.sessionID} inline />
              <ObserverRepairPanel
                sessionID={props.sessionID}
                rootID={root()}
                live={events.repairs}
                escalation={events.escalation}
                inline
              />
            </div>
          </MobilePanelDrawer>
        </Suspense>
      </Show>
    </Show>
  )
}
