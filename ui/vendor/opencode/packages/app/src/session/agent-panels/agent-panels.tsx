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
  // The tree's root owns the observer rows; before it loads the routed session
  // is the only candidate, which is correct for a root session.
  const root = () => (tree.data?.available ? tree.data.value.root : props.sessionID)
  const events = createAgentEvents({ sessionID: () => props.sessionID, root })

  return (
    <>
      <SubagentTreePanel sessionID={props.sessionID} />
      <ObserverRepairPanel sessionID={props.sessionID} live={events.repairs} escalation={events.escalation} />
    </>
  )
}
