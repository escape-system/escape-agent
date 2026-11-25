import type { SessionMessageUser } from "@opencode/client/promise"
import { useLocation, useNavigate } from "@solidjs/router"
import { createEffect, createMemo, onCleanup, onMount } from "solid-js"
import { messageIdFromHash } from "./message-id-from-hash"

export const useSessionHashScroll = (input: {
  sessionKey: () => string
  sessionID: () => string | undefined
  messagesReady: () => boolean
  visibleUserMessages: () => SessionMessageUser[]
  historyMore: () => boolean
  historyLoading: () => boolean
  loadMore: (sessionID: string) => Promise<void>
  currentMessageId: () => string | undefined
  pendingMessage: () => string | undefined
  setPendingMessage: (value: string | undefined) => void
  setActiveMessage: (message: SessionMessageUser | undefined) => void
  follow: { unpin: () => void; toBottom: () => void }
  scroller: () => HTMLDivElement | undefined
  anchor: (id: string) => string
  /** The view restored a mid-history position, so an empty hash must not force the end. */
  restoredScroll?: () => boolean
  revealMessage?: (id: string) => void
  scheduleScrollState: (el: HTMLDivElement) => void
  consumePendingMessage: (key: string) => string | undefined
}) => {
  const visibleUserMessages = createMemo(() => input.visibleUserMessages())
  const messageById = createMemo(() => new Map(visibleUserMessages().map((m) => [m.id, m])))
  let pendingKey = ""
  let clearing = false
  // Targets a load already ran for, so a target history cannot produce does not page forever.
  const loadedTargets = new Set<string>()
  // The last target the view gave up on; it falls back once instead of retrying or stranding at 0.
  let staleTarget = ""

  function staleToBottom(targetId: string) {
    if (staleTarget === targetId) return
    staleTarget = targetId
    clearing = true
    clearMessageHash()
    input.follow.toBottom()
    const el = input.scroller()

    if (el) input.scheduleScrollState(el)
  }

  // Targets belong to one session; a switch must not inherit a previous session's give-up state.
  let targetSession = input.sessionKey()
  const targetsFor = () => {
    const key = input.sessionKey()

    if (key !== targetSession) {
      targetSession = key
      loadedTargets.clear()
      staleTarget = ""
    }

    return key
  }

  const location = useLocation<{ reveal?: string }>()
  const navigate = useNavigate()

  const frames = new Set<number>()

  const queue = (fn: () => void) => {
    const id = requestAnimationFrame(() => {
      frames.delete(id)
      fn()
    })

    frames.add(id)
  }

  const cancel = () => {
    for (const id of frames) cancelAnimationFrame(id)
    frames.clear()
  }

  const clearMessageHash = () => {
    cancel()
    input.consumePendingMessage(input.sessionKey())

    if (input.pendingMessage()) input.setPendingMessage(undefined)

    if (!location.hash) return
    clearing = true
    navigate(location.pathname + location.search, { replace: true })
  }

  const updateHash = (id: string) => {
    const hash = `#${input.anchor(id)}`

    if (location.hash === hash) return
    clearing = false
    navigate(location.pathname + location.search + hash, {
      replace: true,
    })
  }

  const scrollToElement = (el: HTMLElement, behavior: ScrollBehavior) => {
    const root = input.scroller()

    if (!root) return false

    const a = el.getBoundingClientRect()
    const b = root.getBoundingClientRect()
    const sticky = root.querySelector("[data-session-title]")
    const inset = sticky instanceof HTMLElement ? sticky.offsetHeight : 0
    const top = Math.max(0, a.top - b.top + root.scrollTop - inset)
    root.scrollTo({ top, behavior })

    return true
  }

  const seek = (id: string, behavior: ScrollBehavior, left = 4): boolean => {
    input.revealMessage?.(id)
    const el = document.getElementById(input.anchor(id))

    if (el) return scrollToElement(el, behavior)

    if (left <= 0) return false
    queue(() => {
      seek(id, behavior, left - 1)
    })

    return false
  }

  const scrollToMessage = (message: SessionMessageUser, behavior: ScrollBehavior = "smooth") => {
    cancel()

    if (input.currentMessageId() !== message.id) input.setActiveMessage(message)
    // A mounted prompt is close enough to glide to; any other is first brought in by the virtualizer.
    const mounted = behavior === "smooth" ? document.getElementById(input.anchor(message.id)) : null

    if (mounted) scrollToElement(mounted, behavior)

    if (!mounted) {
      input.revealMessage?.(message.id)
      seek(message.id, behavior)
    }

    updateHash(message.id)
  }

  const applyHash = (behavior: ScrollBehavior) => {
    const hash = location.hash.slice(1)

    if (!hash) {
      // A route that opens the session on a tool call leaves the scroll to the timeline, which reveals the call.
      if (location.state?.reveal) return
      // A remount that restored a mid-history position must not be dragged to the end by an empty hash.
      if (input.restoredScroll?.()) return

      input.follow.toBottom()
      const el = input.scroller()

      if (el) input.scheduleScrollState(el)

      return
    }

    const messageId = messageIdFromHash(hash)

    if (messageId) {
      const msg = messageById().get(messageId)

      // Resolve first: unpinning for a target that cannot be shown would strand the reader.
      if (msg) {
        input.follow.unpin()
        scrollToMessage(msg, behavior)

        return
      }

      // History is exhausted, so this target can never appear; the load effect falls back once.
      return
    }

    const target = document.getElementById(hash)

    if (target) {
      input.follow.unpin()
      scrollToElement(target, behavior)

      return
    }

    input.follow.toBottom()
    const el = input.scroller()

    if (el) input.scheduleScrollState(el)
  }

  createEffect(() => {
    const hash = location.hash

    if (!hash) clearing = false

    if (!input.sessionID() || !input.messagesReady()) return
    cancel()
    queue(() => applyHash("auto"))
  })

  createEffect(() => {
    if (!input.sessionID() || !input.messagesReady()) return

    visibleUserMessages()

    let targetId = input.pendingMessage()

    if (!targetId) {
      const key = input.sessionKey()

      if (pendingKey !== key) {
        pendingKey = key
        const next = input.consumePendingMessage(key)

        if (next) {
          input.setPendingMessage(next)
          targetId = next
        }
      }
    }

    if (!targetId && !clearing) targetId = messageIdFromHash(location.hash)

    if (!targetId) return

    const pending = input.pendingMessage() === targetId
    const msg = messageById().get(targetId)

    if (!msg) return

    if (pending) input.setPendingMessage(undefined)

    if (input.currentMessageId() === targetId && !pending) return

    input.follow.unpin()
    cancel()
    queue(() => scrollToMessage(msg, "auto"))
  })

  createEffect(() => {
    const sessionID = input.sessionID()

    if (!sessionID || !input.messagesReady()) return

    // Give-up state belongs to one session; a switch must not inherit it.
    targetsFor()
    visibleUserMessages()

    let targetId = input.pendingMessage()

    if (!targetId && !clearing) targetId = messageIdFromHash(location.hash)

    if (!targetId) return

    if (messageById().has(targetId)) {
      loadedTargets.delete(targetId)
      if (staleTarget === targetId) staleTarget = ""
      return
    }

    // A stale or hidden target must not pin the view: clear it and land on the end once.
    if (!input.historyMore()) return staleToBottom(targetId)

    if (input.historyLoading()) return

    // A load already ran for this target without producing it, so it cannot appear by paging further.
    if (loadedTargets.has(targetId)) return staleToBottom(targetId)
    loadedTargets.add(targetId)
    void input.loadMore(sessionID)
  })

  onMount(() => {
    if (typeof window !== "undefined" && "scrollRestoration" in window.history) {
      window.history.scrollRestoration = "manual"
    }
  })

  onCleanup(cancel)

  return {
    clearMessageHash,
    scrollToMessage,
    applyHash,
  }
}
