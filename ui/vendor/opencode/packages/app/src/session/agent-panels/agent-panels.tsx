import { createEffect, createSignal } from "solid-js"
import { createAgentEvents } from "./agent-events"
import { useSubagentTreeQuery } from "./agent-queries"
import { ObserverRepairPanel } from "./observer-repairs"
import { SubagentTreePanel } from "./subagent-tree"

/**
 * The session header's agent panels (`docs/13-ui.md`): the subagent tree and the
 * observer's repair rows. Both hide themselves when the server has nothing to
 * show, so a session without subagents keeps the plain header.
 */
export function SessionAgentPanels(props: { sessionID: string }) {
  const tree = useSubagentTreeQuery(() => props.sessionID)
  // The tree's root owns the observer rows. Latch it once the tree resolves:
  // letting it fall back to the routed session on every refetch flips the
  // observer query keys, blanks the panel, and resizes the timeline header.
  const [resolvedRoot, setResolvedRoot] = createSignal<string>()

  createEffect(() => {
    const next = tree.data?.available ? tree.data.value.root : undefined

    if (next) setResolvedRoot(next)
  })
  const root = () => resolvedRoot() ?? props.sessionID
  const events = createAgentEvents({ sessionID: () => props.sessionID, root })

  return (
    <>
      <SubagentTreePanel sessionID={props.sessionID} />
      <ObserverRepairPanel
        sessionID={props.sessionID}
        rootID={root()}
        live={events.repairs}
        escalation={events.escalation}
      />
    </>
  )
}
