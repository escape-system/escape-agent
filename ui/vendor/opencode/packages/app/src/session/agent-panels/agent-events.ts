import { createMemo, createSignal, onCleanup, type Accessor } from "solid-js"
import { createStore } from "solid-js/store"
import { isConfigObject } from "@/settings/agentic/config"
import { useServerSDK } from "@/runtime/server/client"
import {
  asCount,
  asText,
  parseGoal,
  parseLaneTicket,
  parseObserverRepaired,
  parsePeerMessage,
  type Goal,
  type LaneTicket,
  type ObserverRepair,
  type PeerMessage,
} from "./agent-wire"

type RawEvent = { type?: string; created?: number; data?: unknown }

/** Live extension state for one session tree, merged into the fetched rows. */
export interface AgentEvents {
  /** `observer.repaired` rows, newest first. */
  repairs: Accessor<ObserverRepair[]>
  /** Set when the server says the repair budget ran out (`observer.escalated`). */
  escalation: Accessor<{ reason: string; budget?: number } | undefined>
  /** `goal.updated` for this tree. */
  goal: Accessor<Goal | undefined>
  /** Queued or running lane tickets owned by this session. */
  activeTickets: Accessor<LaneTicket[]>
}

/** Peer messages delivered to one child session while the drawer is mounted. */
export interface PeerFeed {
  messages: Accessor<PeerMessage[]>
  unread: Accessor<number>
  add(message: PeerMessage): void
  markRead(): void
}

/**
 * Live extension events for one session's panels (`docs/13-ui.md`).
 *
 * The typed SDK event union predates these events, so the stream is read raw
 * and every payload is filtered by the session tree it belongs to: a sibling
 * tree's observer repairs never appear on this screen.
 */
export function createAgentEvents(input: {
  sessionID: () => string | undefined
  root: () => string | undefined
}): AgentEvents {
  const sdk = useServerSDK()
  const [repairs, setRepairs] = createSignal<ObserverRepair[]>([])
  const [escalation, setEscalation] = createSignal<{ reason: string; budget?: number }>()
  const [goal, setGoal] = createSignal<Goal>()
  const [tickets, setTickets] = createStore<Record<string, LaneTicket>>({})

  const ours = (rootID: string | undefined) => !!rootID && (rootID === input.sessionID() || rootID === input.root())

  const stop = sdk.event.listen((event) => {
    const raw = event as unknown as RawEvent
    const sessionID = input.sessionID()
    if (!sessionID || !raw.type) return
    const data = raw.data

    switch (raw.type) {
      case "observer.repaired": {
        if (!isConfigObject(data) || !ours(asText(data.rootID))) return
        const repair = parseObserverRepaired(data)
        if (!repair) return
        setRepairs((current) => [repair, ...current].slice(0, 20))

        return
      }
      case "observer.escalated": {
        if (!isConfigObject(data) || !ours(asText(data.rootID))) return
        setEscalation({ reason: asText(data.reason) ?? "", budget: asCount(data.budget) })

        return
      }
      case "goal.updated": {
        if (!isConfigObject(data) || !ours(asText(data.rootID))) return
        const next = parseGoal(data.goal)
        if (next) setGoal(next)

        return
      }
      case "lane.queued":
      case "lane.started":
      case "lane.finished":
      case "lane.cancelled": {
        const ticket = parseLaneTicket(data)
        // A ticket belongs to the session that submitted it; another session's
        // queue is shown by that session, not here.
        if (!ticket || ticket.owner !== sessionID) return
        setTickets(ticket.id, ticket)

        return
      }
      default:
        return
    }
  })
  onCleanup(stop)

  const activeTickets = createMemo(() =>
    Object.values(tickets).filter((ticket) => ticket.state === "queued" || ticket.state === "running"),
  )

  return { repairs, escalation, goal, activeTickets }
}

/**
 * Peer messages for one child session: `subagent.peer.message` events whose
 * target is this session. There is no history endpoint — a peer message is an
 * inbox row, so the drawer is live-only and starts empty.
 */
export function createPeerFeed(sessionID: () => string | undefined): PeerFeed {
  const sdk = useServerSDK()
  const [messages, setMessages] = createSignal<PeerMessage[]>([])
  const [unread, setUnread] = createSignal(0)

  const add = (message: PeerMessage) => {
    setMessages((current) => (current.some((item) => item.id === message.id) ? current : [...current, message]))
    setUnread((current) => current + 1)
  }

  const stop = sdk.event.listen((event) => {
    const raw = event as unknown as RawEvent
    const id = sessionID()
    if (raw.type !== "subagent.peer.message" || !id) return
    const message = parsePeerMessage(raw.data, id)
    if (message) add(message)
  })
  onCleanup(stop)

  return {
    messages,
    unread,
    add,
    markRead: () => setUnread(0),
  }
}
