/**
 * One compaction, folded from every source that reports it.
 *
 * The timeline used to read two independent sources: the pending inbox list and
 * the `session.compaction.*` message stream. "queued" and "running" could
 * therefore coexist, and a late pending snapshot could resurrect a queued badge
 * after the compaction had already been delivered. This module folds every
 * signal into one record per session so the timeline renders AT MOST ONE
 * compaction row for the compaction in flight and always knows why it ran.
 *
 * The fold is pure and deterministic: it never reads the clock, every signal
 * carries its own `at`, and replaying the same sequence yields the same record.
 */

/** Why a compaction ran. `reason` on the wire stays for compatibility; this carries the true cause. */
export type CompactionTrigger = "guard" | "overflow" | "manual" | "observer" | "command"

export type CompactionPhase = "none" | "suggested" | "queued" | "running" | "done" | "failed"

export type CompactionState = {
  phase: CompactionPhase
  /** The inbox row this compaction occupies, when the server named one. */
  inboxID?: string
  trigger?: CompactionTrigger
  estimateTokens?: number
  triggerTokens?: number
  ceilingTokens?: number
  beforeTokens?: number
  afterTokens?: number
  startedAt?: number
  endedAt?: number
  error?: string
  /** Time of the last signal that changed the record; an older signal cannot move it backwards. */
  at: number
}

/** Every source that can move a compaction forward. `at` is the event's `created`, never `Date.now()`. */
export type CompactionSignal =
  | {
      type: "suggested"
      at: number
      trigger?: CompactionTrigger
      estimateTokens?: number
      triggerTokens?: number
      ceilingTokens?: number
    }
  | {
      type: "queued"
      at: number
      inboxID: string
      trigger?: CompactionTrigger
      estimateTokens?: number
      triggerTokens?: number
      ceilingTokens?: number
    }
  /** A snapshot no longer lists the queued row, with no delivery or cancellation event to explain it. */
  | { type: "dequeued"; at: number; inboxID?: string }
  | {
      type: "started"
      at: number
      inboxID?: string
      trigger?: CompactionTrigger
      estimateTokens?: number
      triggerTokens?: number
      ceilingTokens?: number
    }
  | {
      type: "ended"
      at: number
      inboxID?: string
      trigger?: CompactionTrigger
      beforeTokens?: number
      afterTokens?: number
    }
  | { type: "failed"; at: number; inboxID?: string; trigger?: CompactionTrigger; error?: string }
  | { type: "delivered"; at: number; inboxID?: string }
  | { type: "cancelled"; at: number; inboxID?: string }

export const emptyCompaction: CompactionState = { phase: "none", at: 0 }

/** Phases that own a live row ahead of the transcript. */
export function compactionInFlight(state: CompactionState | undefined): boolean {
  return state?.phase === "suggested" || state?.phase === "queued" || state?.phase === "running"
}

/** Deterministic ordering among signals that share a timestamp: later stages win a tie. */
const signalRank: Record<CompactionSignal["type"], number> = {
  suggested: 0,
  queued: 1,
  started: 2,
  ended: 3,
  failed: 4,
  delivered: 5,
  cancelled: 5,
  dequeued: 6,
}

function signalKey(signal: CompactionSignal) {
  return `${signal.at}\u0000${signalRank[signal.type]}\u0000${signal.inboxID ?? ""}\u0000${signal.type}`
}

/** Sort a signal set into the one order the fold is defined for; ties break on inbox ID and stage. */
export function orderCompactionSignals(signals: readonly CompactionSignal[]): CompactionSignal[] {
  return [...signals].sort((a, b) => (signalKey(a) < signalKey(b) ? -1 : signalKey(a) > signalKey(b) ? 1 : 0))
}

/** Fields that describe the compaction in flight, not the one this record already finished. */
const livePhases: readonly CompactionPhase[] = ["suggested", "queued", "running"]

function carryFields(
  state: CompactionState,
  signal: { trigger?: CompactionTrigger } & Record<string, unknown>,
) {
  // Numbers and the trigger describe one compaction. Carrying them past `done`/`failed` would label
  // the next compaction with the previous one's measurements.
  const continuing = livePhases.includes(state.phase)
  const trigger = signal.trigger ?? (continuing ? state.trigger : undefined)
  const pick = (key: "estimateTokens" | "triggerTokens" | "ceilingTokens" | "beforeTokens" | "afterTokens") => {
    const next = signal[key]
    if (typeof next === "number") return next
    return continuing ? state[key] : undefined
  }
  const estimateTokens = pick("estimateTokens")
  const triggerTokens = pick("triggerTokens")
  const ceilingTokens = pick("ceilingTokens")
  const beforeTokens = pick("beforeTokens")
  const afterTokens = pick("afterTokens")

  return {
    ...(trigger !== undefined ? { trigger } : {}),
    ...(estimateTokens !== undefined ? { estimateTokens } : {}),
    ...(triggerTokens !== undefined ? { triggerTokens } : {}),
    ...(ceilingTokens !== undefined ? { ceilingTokens } : {}),
    ...(beforeTokens !== undefined ? { beforeTokens } : {}),
    ...(afterTokens !== undefined ? { afterTokens } : {}),
  }
}

/** Whether a signal describes a different compaction than the one already in flight. */
function otherCompaction(state: CompactionState, signal: { inboxID?: string }) {
  if (state.phase !== "queued" && state.phase !== "running") return false
  if (!signal.inboxID || !state.inboxID) return false

  return signal.inboxID !== state.inboxID
}

/**
 * Fold one signal into the record. Pure: no clock, no I/O, no mutation of
 * `previous`. A signal older than the record's last change is dropped, so an
 * event that overtakes a snapshot cannot move the phase backwards.
 */
export function foldCompaction(previous: CompactionState, signal: CompactionSignal): CompactionState {
  const state = previous ?? emptyCompaction
  const sameInbox = !!signal.inboxID && signal.inboxID === state.inboxID
  const newer = signal.at > state.at
  const sameTick = signal.at === state.at

  switch (signal.type) {
    case "suggested": {
      // A suggestion is actionable only while nothing is already queued or running.
      if (state.phase === "queued" || state.phase === "running") return state
      if (!newer) return state

      return { phase: "suggested", at: signal.at, ...carryFields(state, signal) }
    }
    case "queued": {
      // Idempotent per inbox ID: a replayed admission never creates a second row, and a row already
      // delivered, running or finished is not queued again by a stale snapshot.
      if (sameInbox && !newer) return state
      if (otherCompaction(state, signal) && !newer) return state
      if (state.phase === "running" && sameInbox) return state

      return {
        phase: "queued",
        inboxID: signal.inboxID,
        at: Math.max(signal.at, state.at),
        ...carryFields(state, signal),
      }
    }
    case "dequeued": {
      if (state.phase !== "queued") return state
      if (signal.inboxID && !sameInbox) return state
      if (!newer) return state

      return { phase: "none", at: signal.at }
    }
    case "started": {
      if (otherCompaction(state, signal) && !newer) return state
      if (sameInbox && state.phase === "running") return state
      if (!newer && !sameInbox) return state
      // A stale `started` for a compaction that already reported its end cannot restart it.
      if (!newer && (state.phase === "done" || state.phase === "failed")) return state

      return {
        phase: "running",
        at: Math.max(signal.at, state.at),
        ...(signal.inboxID !== undefined ? { inboxID: signal.inboxID } : state.inboxID !== undefined ? { inboxID: state.inboxID } : {}),
        ...carryFields(state, signal),
        startedAt: livePhases.includes(state.phase) && state.startedAt !== undefined ? state.startedAt : signal.at,
      }
    }
    case "ended": {
      if (otherCompaction(state, signal) && !newer) return state
      if (!newer && !sameInbox) return state
      if (state.phase === "done" && sameInbox) return state
      if (!newer && state.phase === "done") return state

      return {
        phase: "done",
        at: Math.max(signal.at, state.at),
        ...(signal.inboxID !== undefined ? { inboxID: signal.inboxID } : state.inboxID !== undefined ? { inboxID: state.inboxID } : {}),
        ...carryFields(state, signal),
        ...(livePhases.includes(state.phase) && state.startedAt !== undefined ? { startedAt: state.startedAt } : {}),
        endedAt: signal.at,
      }
    }
    case "failed": {
      if (otherCompaction(state, signal) && !newer) return state
      if (!newer && !sameInbox) return state
      if (state.phase === "failed" && sameInbox) return state

      return {
        phase: "failed",
        at: Math.max(signal.at, state.at),
        ...(signal.inboxID !== undefined ? { inboxID: signal.inboxID } : state.inboxID !== undefined ? { inboxID: state.inboxID } : {}),
        ...carryFields(state, signal),
        ...(livePhases.includes(state.phase) && state.startedAt !== undefined ? { startedAt: state.startedAt } : {}),
        ...(signal.error ? { error: signal.error } : {}),
        endedAt: signal.at,
      }
    }
    case "delivered":
    case "cancelled": {
      // Retire only the queued badge. A running or finished compaction keeps its row.
      if (state.phase !== "queued") return state
      if (signal.inboxID && !sameInbox) return state
      if (!newer && !sameTick) return state

      return { phase: "none", at: Math.max(signal.at, state.at) }
    }
  }
}

/** Fold a whole sequence from a known start, in the canonical order. */
export function compactionFrom(signals: readonly CompactionSignal[], initial: CompactionState = emptyCompaction) {
  return orderCompactionSignals(signals).reduce(foldCompaction, initial)
}

/** A stable identity for the reducer's row; rows are per session, so the phase is enough. */
export function compactionRowKey(state: CompactionState): string {
  return `compaction:${state.phase}`
}

/** The parts of the store a compaction is visible in; typed structurally so this module stays UI-only. */
export type CompactionViewInput = {
  pending?: readonly { id?: string; type?: string; time?: { created?: number } }[]
  messages?: readonly { id?: string; type?: string; status?: string; time?: { created?: number } }[]
}

/**
 * The signals the timeline can see without live events: a pending `compaction` inbox row, and a
 * compaction message row that survived a reload. Live-event signals are folded on top, so a session
 * opened mid-compaction still shows one correct row.
 */
export function compactionViewSignals(input: CompactionViewInput): CompactionSignal[] {
  const signals: CompactionSignal[] = []
  input.pending?.forEach((item) => {
    if (item.type !== "compaction" || !item.id) return
    signals.push({ type: "queued", at: item.time?.created ?? 0, inboxID: item.id })
  })
  input.messages?.forEach((message) => {
    if (message.type !== "compaction" || !message.id) return
    const at = message.time?.created ?? 0
    if (message.status === "running") signals.push({ type: "started", at, inboxID: message.id })
    else if (message.status === "completed") signals.push({ type: "ended", at, inboxID: message.id })
    else if (message.status === "failed") signals.push({ type: "failed", at, inboxID: message.id })
  })

  return signals
}

/** Fold the durable view and the live signals into one record, deterministically. */
export function foldCompactionView(
  initial: CompactionState,
  view: CompactionViewInput,
  live: readonly CompactionSignal[] = [],
): CompactionState {
  return compactionFrom([...compactionViewSignals(view), ...live], initial)
}

/** The human-readable reason, used by the timeline row and the completion notice. */
export function compactionReasonKey(trigger: CompactionTrigger | undefined) {
  switch (trigger) {
    case "guard":
      return "ui.messagePart.compaction.why.guard" as const
    case "overflow":
      return "ui.messagePart.compaction.why.overflow" as const
    case "manual":
      return "ui.messagePart.compaction.why.manual" as const
    case "observer":
      return "ui.messagePart.compaction.why.observer" as const
    case "command":
      return "ui.messagePart.compaction.why.command" as const
    default:
      return "ui.messagePart.compaction.why.unknown" as const
  }
}
