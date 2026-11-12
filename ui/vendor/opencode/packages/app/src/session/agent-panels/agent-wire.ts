import { isConfigObject } from "@/settings/agentic/config"

/**
 * Wire values for the experimental agent panels (docs/13-ui.md).
 *
 * These routes are extensions: every parser ignores unknown fields and returns
 * `undefined` for a value it cannot read, so an older or newer server can never
 * make the session screen throw.
 */

/** `Store::session_node` derives exactly these states. */
export type SubagentState = "busy" | "idle" | "suspended" | "failed" | "interrupted" | "archived"

export type SubagentNode = {
  id: string
  parentID?: string
  agent: string
  model?: string
  /** Derived lifecycle state; see `Store::session_node`. */
  state: SubagentState
  depth: number
  role: string
  spawnAllowed: boolean
  childCount: number
}

export type SubagentTree = { root: string; nodes: SubagentNode[] }

/** One observer repair, from `GET /api/experimental/observer/repairs` or `observer.repaired`. */
export type ObserverRepair = {
  id?: string
  kind: string
  reason: string
  /** Repaired session, tool call or message. */
  target: string
  attempt: number
  created?: number
}

export type ObserverBudget = {
  used: number
  limit: number
  exhausted: boolean
}

export type ObserverState = { budget?: ObserverBudget }

export type Goal = {
  id: string
  rootID: string
  objective: string
  criteria: string[]
  status: string
  rounds: number
  stalls: number
  evidence: string[]
  /** Criteria the server reports as satisfied; absent on rows that predate it. */
  done?: number
  /** Total criteria the server reports; absent on rows that predate it. */
  total?: number
  model?: string
}

export type LaneStatus = { lane: string; concurrency: number; running: number; queued: number }

export type LaneTicket = {
  id: string
  lane: string
  command: string
  /** The owning session, from `ownerSession`; `undefined` when the submitter was unknown. */
  owner?: string
  state: "queued" | "running" | "succeeded" | "failed" | "cancelled"
  queued: number
}

export type PeerMessage = {
  /** Inbox row id when the event carries one; used to fold the POST echo into the same row. */
  id: string
  from: string
  /** Sender agent id, when the event names one. */
  agent?: string
  /** `note`, `question` or `answer` for `talk`; absent for a hand-written message. */
  kind?: string
  /** Written from this drawer, not delivered by a peer. */
  own?: boolean
  text: string
  created: number
}

/** A non-empty string field, or `undefined` when it is absent or mistyped. */
export const asText = (value: unknown) => (typeof value === "string" && value ? value : undefined)
/** A finite numeric field, or `undefined` when it is absent or mistyped. */
export const asCount = (value: unknown) => (typeof value === "number" && Number.isFinite(value) ? value : undefined)
const list = (value: unknown) => (Array.isArray(value) ? value : [])

export function parseSubagentTree(value: unknown): SubagentTree | undefined {
  if (!isConfigObject(value)) return undefined
  const root = asText(value.root)
  if (!root) return undefined

  return { root, nodes: list(value.nodes).flatMap((node) => parseSubagentNode(node) ?? []) }
}

function parseSubagentNode(value: unknown): SubagentNode | undefined {
  if (!isConfigObject(value)) return undefined
  const id = asText(value.id)
  if (!id) return undefined

  return {
    id,
    parentID: asText(value.parentID),
    agent: asText(value.agent) ?? "build",
    model: asText(value.model),
    state: subagentState(value.state),
    depth: asCount(value.depth) ?? 0,
    role: asText(value.role) ?? "primary",
    spawnAllowed: value.spawnAllowed === true,
    childCount: asCount(value.childCount) ?? 0,
  }
}

/** Unknown states (a newer server) fall back to `idle`, never to a fake activity. */
function subagentState(value: unknown): SubagentState {
  const state = asText(value)
  switch (state) {
    case "busy":
    case "suspended":
    case "failed":
    case "interrupted":
    case "archived":
      return state
    default:
      return "idle"
  }
}

export function parseObserverRepairs(value: unknown): ObserverRepair[] {
  return list(value).flatMap((row) => {
    if (!isConfigObject(row)) return []
    const kind = asText(row.kind)
    if (!kind) return []

    return [
      {
        id: asText(row.id),
        kind,
        reason: asText(row.reason) ?? "",
        target: asText(row.targetID) ?? asText(row.target) ?? "",
        attempt: asCount(row.attempt) ?? 0,
        created: timeOf(row.time),
      },
    ]
  })
}

/** `observer.repaired` carries no id and no timestamp: it is the newest row of the audit. */
export function parseObserverRepaired(value: unknown): ObserverRepair | undefined {
  if (!isConfigObject(value)) return undefined
  const kind = asText(value.kind)
  if (!kind) return undefined

  return {
    kind,
    reason: asText(value.reason) ?? "",
    target: asText(value.target) ?? asText(value.targetID) ?? "",
    attempt: asCount(value.attempt) ?? 0,
  }
}

export function parseObserverState(value: unknown): ObserverState {
  if (!isConfigObject(value)) return {}

  const budget = isConfigObject(value.budget)
    ? {
        used: asCount(value.budget.rootUsed) ?? 0,
        limit: asCount(value.budget.rootLimit) ?? 0,
        exhausted: value.budget.exhausted === true,
      }
    : undefined

  return { budget }
}

export function parseGoal(value: unknown): Goal | undefined {
  if (!isConfigObject(value)) return undefined
  const id = asText(value.id)
  const objective = asText(value.objective)
  if (!id || !objective) return undefined

  return {
    id,
    rootID: asText(value.rootID) ?? "",
    objective,
    criteria: list(value.criteria).filter((item): item is string => typeof item === "string"),
    status: asText(value.status) ?? "active",
    rounds: asCount(value.rounds) ?? 0,
    stalls: asCount(value.stalls) ?? 0,
    evidence: list(value.evidence).filter((item): item is string => typeof item === "string"),
    done: asCount(value.done),
    total: asCount(value.total),
    model: asText(value.model),
  }
}

export function parseLaneStatuses(value: unknown): LaneStatus[] {
  return list(value).flatMap((row) => {
    if (!isConfigObject(row)) return []
    const lane = asText(row.lane)
    if (!lane) return []

    return [
      {
        lane,
        concurrency: asCount(row.concurrency) ?? 0,
        running: asCount(row.running) ?? 0,
        queued: asCount(row.queued) ?? 0,
      },
    ]
  })
}

/**
 * One lane ticket event: `{lane, ticket, sessionID?}` where `ticket` is the
 * full ticket object (`lane.queued|started|finished|cancelled`).
 */
export function parseLaneTicket(value: unknown): LaneTicket | undefined {
  if (!isConfigObject(value)) return undefined
  const ticket = isConfigObject(value.ticket) ? value.ticket : value
  const id = asText(ticket.id)
  const lane = asText(value.lane) ?? asText(ticket.lane)
  if (!id || !lane) return undefined

  return {
    id,
    lane,
    command: asText(ticket.command) ?? "",
    owner: asText(value.sessionID) ?? asText(ticket.ownerSession),
    state: ticketState(ticket.state),
    queued: asCount(ticket.queuedMs) ?? 0,
  }
}

function ticketState(value: unknown): LaneTicket["state"] {
  const tag = isConfigObject(value) ? asText(value.state) : asText(value)
  switch (tag) {
    case "queued":
    case "running":
    case "succeeded":
    case "failed":
    case "cancelled":
      return tag
    default:
      return "queued"
  }
}

/**
 * `subagent.peer.message`. The field names are matched loosely on purpose:
 * the event is an extension and the drawer must not lose a message to a rename.
 */
export function parsePeerMessage(value: unknown, sessionID: string): PeerMessage | undefined {
  if (!isConfigObject(value)) return undefined

  const target = asText(value.sessionID) ?? asText(value.target) ?? asText(value.targetID) ?? asText(value.to)
  if (target !== sessionID) return undefined

  const payload = isConfigObject(value.payload) ? value.payload : undefined
  const textBody = asText(value.text) ?? asText(value.message) ?? (payload ? asText(payload.text) : undefined)
  if (!textBody) return undefined

  const from =
    asText(value.from) ??
    asText(value.fromID) ??
    asText(value.fromSessionID) ??
    asText(value.sender) ??
    asText(value.source) ??
    "peer"
  const created = asCount(value.created) ?? timeOf(value.time) ?? Date.now()
  const id = asText(value.messageID) ?? asText(value.id) ?? `${from}:${created}`

  return {
    id,
    from,
    agent: asText(value.agent),
    kind: asText(value.kind),
    text: textBody,
    created,
  }
}

function timeOf(value: unknown) {
  return isConfigObject(value) ? asCount(value.created) : undefined
}
