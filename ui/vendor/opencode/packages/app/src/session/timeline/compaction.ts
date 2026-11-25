import { createSignal, onCleanup, type Accessor } from "solid-js"
import {
  compactionFrom,
  foldCompactionView,
  type CompactionSignal,
  type CompactionState,
  type CompactionTrigger,
} from "@opencode/session-ui/timeline/compaction"
import { useServer } from "@/runtime/server/current"

const triggers: readonly CompactionTrigger[] = ["guard", "overflow", "manual", "observer", "command"]

const asTrigger = (value: unknown): CompactionTrigger | undefined =>
  typeof value === "string" && (triggers as readonly string[]).includes(value)
    ? (value as CompactionTrigger)
    : undefined

const asCount = (value: unknown) => (typeof value === "number" && Number.isFinite(value) ? value : undefined)

type RawEvent = { type: string; created: number; data?: unknown }
type CompactionEventData = {
  sessionID?: unknown
  inboxID?: unknown
  inputID?: unknown
  trigger?: unknown
  estimateTokens?: unknown
  triggerTokens?: unknown
  ceilingTokens?: unknown
  beforeTokens?: unknown
  afterTokens?: unknown
  error?: unknown
}

const text = (value: unknown) => (typeof value === "string" && value ? value : undefined)

/**
 * The session's compaction record, folded from the live event stream on top of the durable store
 * view. One record drives the one timeline row, so a queued admission and a running compaction can
 * never render together and a stale pending snapshot cannot resurrect a badge that was delivered.
 *
 * Every signal carries the event's own `created`, so replaying the same stream yields the same
 * record; nothing here reads the clock.
 */
export function createSessionCompaction(input: { sessionID: Accessor<string | undefined> }) {
  const server = useServer()
  const [signals, setSignals] = createSignal<Record<string, CompactionSignal[]>>({})

  // Bounded per session: an old log cannot grow without limit, and a compaction has few signals.
  const append = (sessionID: string, signal: CompactionSignal) =>
    setSignals((current) => ({ ...current, [sessionID]: [...(current[sessionID] ?? []), signal].slice(-32) }))

  const stop = server.ctx.sdk.event.listen((raw) => {
    const event = raw as unknown as RawEvent
    const data = (typeof event.data === "object" && event.data !== null ? event.data : {}) as CompactionEventData
    const sessionID = text(data.sessionID)
    if (!sessionID) return

    const inboxID = text(data.inboxID) ?? text(data.inputID)
    const trigger = asTrigger(data.trigger)
    const tokens = () => ({
      ...(asCount(data.estimateTokens) !== undefined ? { estimateTokens: asCount(data.estimateTokens)! } : {}),
      ...(asCount(data.triggerTokens) !== undefined ? { triggerTokens: asCount(data.triggerTokens)! } : {}),
      ...(asCount(data.ceilingTokens) !== undefined ? { ceilingTokens: asCount(data.ceilingTokens)! } : {}),
    })

    switch (event.type) {
      case "session.inbox.delivered":
      case "session.inbox.cancelled": {
        const id = text(data.inboxID)
        if (!id) return
        append(sessionID, {
          type: event.type === "session.inbox.delivered" ? "delivered" : "cancelled",
          at: event.created,
          inboxID: id,
        })

        return
      }
      case "session.compaction.suggested":
        append(sessionID, {
          type: "suggested",
          at: event.created,
          trigger: trigger ?? "observer",
          ...tokens(),
        })

        return
      case "session.compaction.started":
        append(sessionID, {
          type: "started",
          at: event.created,
          ...(inboxID ? { inboxID } : {}),
          ...(trigger ? { trigger } : {}),
          ...tokens(),
        })

        return
      case "session.compaction.ended":
        append(sessionID, {
          type: "ended",
          at: event.created,
          ...(inboxID ? { inboxID } : {}),
          ...(trigger ? { trigger } : {}),
          ...(asCount(data.beforeTokens) !== undefined ? { beforeTokens: asCount(data.beforeTokens)! } : {}),
          ...(asCount(data.afterTokens) !== undefined ? { afterTokens: asCount(data.afterTokens)! } : {}),
        })

        return
      case "session.compaction.failed": {
        const error =
          typeof data.error === "string"
            ? data.error
            : typeof (data.error as { message?: unknown } | undefined)?.message === "string"
              ? String((data.error as { message: string }).message)
              : undefined
        append(sessionID, {
          type: "failed",
          at: event.created,
          ...(inboxID ? { inboxID } : {}),
          ...(trigger ? { trigger } : {}),
          ...(error ? { error } : {}),
        })

        return
      }
      // `session.compaction.delta` is not implemented by the server (docs/02-protocol.md): streaming
      // summary text has no event, so a running compaction's summary arrives with `ended`.
      default:
        return
    }
  })
  onCleanup(stop)

  /**
   * The durable view is the pending row plus the compaction message that survived a reload; live
   * signals stack on top. Folding from scratch keeps the record a function of current state, so a
   * row that leaves the inbox cannot leave a badge behind.
   */
  const state = (): CompactionState => {
    const id = input.sessionID()
    if (!id) return compactionFrom([])

    const view = {
      pending: server.ctx.data.session.pending
        .list(id)
        .flatMap((item) => (item.type === "compaction" ? [{ id: item.id, type: "compaction", time: item.time }] : [])),
      messages: server.ctx.data.session.message.list(id).flatMap((message) =>
        message.type === "compaction"
          ? [
              {
                id: message.id,
                type: "compaction",
                status: message.status,
                time: message.time,
                ...(message.reason === "manual" ? { trigger: "manual" as const } : {}),
              },
            ]
          : [],
      ),
    }

    return foldCompactionView(compactionFrom([]), view, signals()[id] ?? [])
  }

  return { state }
}
