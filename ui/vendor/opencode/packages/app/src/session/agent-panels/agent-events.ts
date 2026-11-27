import { createEffect, createMemo, createSignal, onCleanup, type Accessor } from "solid-js"
import { createStore } from "solid-js/store"
import { isConfigObject } from "@/settings/agentic/config"
import { useServerSDK } from "@/runtime/server/client"
import { useServer } from "@/runtime/server/current"
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

/** A peer message plus its place in the inbox/transcript lifecycle. */
export type PeerRow = PeerMessage & { status?: "queued" | "sent"; delivery?: "queue" | "steer" }

/** One Q&A entry in the peer drawer: a prompt/peer question or a subagent assistant reply. */
export type PeerThreadRow =
  | (PeerRow & { role: "question"; origin: "user" | "peer" })
  | {
      id: string
      role: "answer"
      text: string
      agent?: string
      created: number
      streaming?: boolean
      error?: string
    }

/** Peer messages for one child session, hydrated from the durable inbox and transcript. */
export interface PeerFeed {
  /** Delivered rows and queued rows, chronological. */
  messages: Accessor<PeerRow[]>
  /** Chronological Q&A thread (questions + subagent assistant replies). */
  thread?: Accessor<PeerThreadRow[]>
  /** Rows still waiting in the session inbox (`status === "queued"`). */
  queued: Accessor<PeerRow[]>
  unread: Accessor<number>
  add(message: PeerMessage, delivery?: "queue" | "steer"): void
  markRead(): void
  steer?(inboxID: string): Promise<void>
  cancel?(inboxID: string): Promise<void>
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
 * Peer messages for one child session.
 *
 * A peer post is a durable inbox row: `POST …/subagent/:sid/message` enqueues a
 * synthetic item carrying `payload.peer` and `metadata.source === "peer"`, and
 * the delivery promotes it into the transcript under the same id. The drawer
 * therefore reads the store twice — `pending.list` for rows still queued and
 * `message.list` for delivered rows — and keeps only the live event stream for
 * arrivals it has not hydrated yet. Ids come from the inbox row, so the POST
 * echo and `subagent.peer.message` fold into one row instead of duplicating it.
 */
export function createPeerFeed(sessionID: () => string | undefined): PeerFeed {
  const sdk = useServerSDK()
  const server = useServer()
  const [live, setLive] = createSignal<PeerRow[]>([])
  const [unread, setUnread] = createSignal(0)
  const [seen, setSeen] = createStore<Record<string, true>>({})

  createEffect(() => {
    const id = sessionID()
    if (!id) return
    const pending = server.ctx.data.session.pending as { hydrate?: (sessionID: string) => Promise<unknown> }
    const messages = server.ctx.data.session.message as { hydrate?: (sessionID: string) => Promise<unknown> }
    void pending.hydrate?.(id)?.catch(() => undefined)
    void messages.hydrate?.(id)?.catch(() => undefined)
  })

  // Live arrivals are held in a signal array: a `createStore` record read with
  // `Object.values` is not tracked by the memo below, which left event-only rows
  // invisible until the next store write.
  const add = (message: PeerMessage, delivery?: "queue" | "steer") => {
    const entry: PeerRow = { ...message, status: "queued", ...(delivery ? { delivery } : {}) }
    setSeen(message.id, true)
    setLive((current) => (current.some((row) => row.id === message.id) ? current : [...current, entry]))
    if (!message.own) setUnread((current) => current + 1)
  }

  const stop = sdk.event.listen((event) => {
    const raw = event as unknown as RawEvent
    const id = sessionID()
    if (!id) return
    if (raw.type === "server.connected") {
      // Any queued placeholder captured before an SSE drop is authoritative on
      // reconnect only if the server's hydrated inbox/transcript still has it.
      setLive([])
      const pending = server.ctx.data.session.pending as { hydrate?: (sessionID: string) => Promise<unknown> }
      const messages = server.ctx.data.session.message as { hydrate?: (sessionID: string) => Promise<unknown> }
      void pending.hydrate?.(id)?.catch(() => undefined)
      void messages.hydrate?.(id)?.catch(() => undefined)
      return
    }
    if (
      (raw.type === "session.inbox.delivered" || raw.type === "session.inbox.cancelled") &&
      isConfigObject(raw.data) &&
      raw.data.sessionID === id
    ) {
      const inboxID = asText(raw.data.inboxID)
      if (inboxID) setLive((current) => current.filter((row) => row.id !== inboxID))
      return
    }
    if (raw.type !== "subagent.peer.message") return
    const message = parsePeerMessage(raw.data, id)
    if (message && !seen[message.id]) {
      setSeen(message.id, true)
      add(message)
    }
  })
  onCleanup(stop)

  // A plain accessor, not `createMemo`: under the test/store mock the memo's
  // computation ran once at creation and never re-subscribed, so live rows never
  // reached the drawer. Reading on demand is always current; the snapshot cache
  // below keeps repeated reads between changes O(1) while invalidating on in-place mutations.
  let cacheKey: [string, unknown, number, string, unknown, number, string, PeerRow[]] | undefined
  let cacheValue: PeerRow[] = []
  let cacheThread: PeerThreadRow[] = []

  const snapshot = () => {
    const id = sessionID()
    if (!id) return { messages: [] as PeerRow[], thread: [] as PeerThreadRow[] }
    const session = server.ctx.data.session
    const pending = session.pending.list(id)
    const transcript = session.message.list(id)
    const liveRows = live()
    const deliverySignature = pending
      .map((item) =>
        isConfigObject(item)
          ? `${asText(item.id) ?? ""}:${asText(item.delivery) ?? ""}:${createdOf(item) ?? 0}`
          : "",
      )
      .join(",")
    const transcriptSignature = transcript
      .map((item) => {
        if (!isConfigObject(item)) return ""
        const rawItem = item as Record<string, unknown>
        const content = Array.isArray(rawItem.content) ? rawItem.content : undefined
        const contentSig = content
          ? content
              .map((part) => {
                if (!isConfigObject(part)) return ""
                const p = part as Record<string, unknown>
                const pType = asText(p.type) ?? ""
                const textLen = typeof p.text === "string" ? p.text.length : 0
                const state = isConfigObject(p.state) ? (asText(p.state.status) ?? "") : ""
                const outLen =
                  isConfigObject(p.state) && typeof p.state.output === "string"
                    ? p.state.output.length
                    : isConfigObject(p.state) && Array.isArray(p.state.content)
                      ? p.state.content.length
                      : 0
                return `${pType}:${textLen}:${state}:${outLen}`
              })
              .join(",")
          : ""
        const textLen = typeof rawItem.text === "string" ? rawItem.text.length : 0
        const time = isConfigObject(rawItem.time) ? (rawItem.time as Record<string, unknown>) : undefined
        const streamed = time ? (asCount(time.streamed) ?? -1) : -1
        const completed = time ? (asCount(time.completed) ?? -1) : -1
        const cancelled = isConfigObject(rawItem.metadata) && rawItem.metadata.cancelled === true ? 1 : 0
        const err = isConfigObject(rawItem.error) ? (asText(rawItem.error.message)?.length ?? 1) : 0
        return `${asText(rawItem.id) ?? ""}:${createdOf(rawItem) ?? 0}:${textLen}:${contentSig}:${streamed}:${completed}:${cancelled}:${err}`
      })
      .join("|")

    if (
      cacheKey &&
      cacheKey[0] === id &&
      cacheKey[1] === pending &&
      cacheKey[2] === pending.length &&
      cacheKey[3] === deliverySignature &&
      cacheKey[4] === transcript &&
      cacheKey[5] === transcript.length &&
      cacheKey[6] === transcriptSignature &&
      cacheKey[7] === liveRows
    )
      return { messages: cacheValue, thread: cacheThread }

    const rows = new Map<string, PeerRow>()
    const questions = new Map<string, PeerThreadRow & { role: "question" }>()
    const answers: (PeerThreadRow & { role: "answer" })[] = []
    const pendingIDs = new Set<string>()

    // Delivered transcript rows first, then live SSE events not yet in transcript,
    // and finally active inbox rows so any item still in `session_inbox` (including
    // materialized `steer` rows) accurately keeps `status: "queued"` until delivered.
    for (const item of transcript) {
      const peer = deliveredRow(item)
      if (peer) {
        rows.set(peer.id, peer)
        questions.set(peer.id, { ...peer, role: "question", origin: "peer" })
        continue
      }
      const user = userQuestionRow(item)
      if (user) {
        questions.set(user.id, user)
        continue
      }
      const answer = assistantAnswerRow(item)
      if (answer) answers.push(answer)
    }
    for (const message of liveRows) {
      pendingIDs.add(message.id)
      if (!rows.has(message.id)) {
        const entry: PeerRow = { ...message, status: "queued" }
        rows.set(message.id, entry)
        questions.set(message.id, { ...entry, role: "question", origin: "peer" })
      }
    }
    for (const item of pending) {
      const row = queuedRow(item)
      if (row) {
        pendingIDs.add(row.id)
        rows.set(row.id, row)
        questions.set(row.id, { ...row, role: "question", origin: "peer" })
      }
    }

    const sortChrono = <T extends { created: number; id: string }>(a: T, b: T) =>
      a.created - b.created || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0)

    const sortedMessages = [...rows.values()].sort(sortChrono)
    const deliveredThread = [
      ...[...questions.values()].filter((row) => !pendingIDs.has(row.id)),
      ...answers,
    ].sort(sortChrono)
    const pendingThread = [...questions.values()].filter((row) => pendingIDs.has(row.id)).sort(sortChrono)

    cacheKey = [
      id,
      pending,
      pending.length,
      deliverySignature,
      transcript,
      transcript.length,
      transcriptSignature,
      liveRows,
    ]
    cacheValue = sortedMessages
    cacheThread = [...deliveredThread, ...pendingThread]

    return { messages: cacheValue, thread: cacheThread }
  }

  const messages = () => snapshot().messages
  const thread = () => snapshot().thread

  const queued = () => messages().filter((row) => row.status === "queued")

  const steer = async (inboxID: string) => {
    const id = sessionID()
    if (!id) return
    await server.ctx.sdk.api.session.inbox.update({ sessionID: id, inboxID, delivery: "steer" })
  }

  const cancel = async (inboxID: string) => {
    const id = sessionID()
    if (!id) return
    await server.ctx.sdk.api.session.inbox.cancel({ sessionID: id, inboxID })
    setLive((current) => current.filter((row) => row.id !== inboxID))
  }

  return {
    messages,
    thread,
    queued,
    unread,
    add,
    markRead: () => setUnread(0),
    steer,
    cancel,
  }
}

/** `payload.peer` or `payload.metadata.source === "peer"`, on an inbox or transcript row. */
function isPeerPayload(value: unknown): boolean {
  if (!isConfigObject(value)) return false
  if (value.peer === true) return true

  return isConfigObject(value.metadata) && value.metadata.source === "peer"
}

function senderOf(payload: unknown, fallback: unknown): string {
  if (isConfigObject(payload) && isConfigObject(payload.metadata)) {
    const from = asText(payload.metadata.from)
    if (from) return from
  }

  return asText(fallback) ?? "peer"
}

/** One queued inbox row (`user` or `synthetic`) that was posted as a peer message. */
function queuedRow(item: unknown): PeerRow | undefined {
  if (!isConfigObject(item)) return undefined
  const type = asText(item.type)
  if (type !== "user" && type !== "synthetic") return undefined
  if (!isPeerPayload(item.payload)) return undefined
  const id = asText(item.id)
  const payload = isConfigObject(item.payload) ? item.payload : undefined
  const text = payload ? asText(payload.text) : undefined
  if (!id || !text) return undefined

  const from = senderOf(payload, "peer")
  const delivery = item.delivery === "steer" || item.delivery === "queue" ? item.delivery : undefined

  return {
    id,
    from,
    agent: payload && isConfigObject(payload.metadata) ? asText(payload.metadata.agent) : undefined,
    kind: payload && isConfigObject(payload.metadata) ? asText(payload.metadata.kind) : undefined,
    own: from === "user",
    text,
    created: createdOf(item) ?? 0,
    status: "queued",
    delivery,
  }
}

/** One delivered transcript row: `synthetic` (current) or legacy `peer`. */
function deliveredRow(item: unknown): PeerRow | undefined {
  if (!isConfigObject(item)) return undefined
  const type = asText(item.type)
  if (type !== "synthetic" && type !== "peer") return undefined
  const id = asText(item.id)
  const text = asText(item.text)
  if (!id || !text) return undefined
  // The promoted body keeps the inbox payload fields at the top level
  // (`peer`, `metadata.source`), and some servers carry them under `payload`.
  const marker = isPeerPayload(item) ? item : isConfigObject(item.payload) ? item.payload : undefined
  if (!marker) return undefined
  const metadata = isConfigObject(item.metadata) ? item.metadata : undefined
  const own = metadata?.from === "user" || (isConfigObject(item.payload) && senderOf(item.payload, undefined) === "user")

  return {
    id,
    from: senderOf(item, metadata?.from),
    agent: metadata ? asText(metadata.agent) : undefined,
    kind: metadata ? asText(metadata.kind) : undefined,
    own,
    text,
    created: createdOf(item) ?? 0,
    status: "sent",
  }
}

function createdOf(item: unknown): number | undefined {
  if (!isConfigObject(item) || !isConfigObject(item.time)) return undefined

  return asCount(item.time.created)
}

function userQuestionRow(item: unknown): (PeerThreadRow & { role: "question" }) | undefined {
  if (!isConfigObject(item) || item.type !== "user") return undefined
  const id = asText(item.id)
  const text = asText(item.text)
  if (!id || !text) return undefined

  return {
    id,
    role: "question",
    origin: "user",
    from: "user",
    own: true,
    text,
    created: createdOf(item) ?? 0,
    status: "sent",
  }
}

function assistantAnswerRow(item: unknown): (PeerThreadRow & { role: "answer" }) | undefined {
  if (!isConfigObject(item) || item.type !== "assistant") return undefined
  const id = asText(item.id)
  if (!id) return undefined
  const parts = Array.isArray(item.content) ? item.content : []
  const texts = parts.flatMap((part) => {
    if (!isConfigObject(part) || part.type !== "text") return []
    const text = asText(part.text)
    return text ? [text] : []
  })
  const text = texts.join("\n\n")
  const time = isConfigObject(item.time) ? item.time : undefined
  const streaming = time ? asCount(time.completed) === undefined : false
  const error = isConfigObject(item.error) ? asText(item.error.message) : undefined

  if (!text && !error && !streaming) return undefined

  return {
    id,
    role: "answer",
    text: text || error || "",
    agent: asText(item.agent),
    created: createdOf(item) ?? 0,
    streaming,
    ...(error ? { error } : {}),
  }
}
