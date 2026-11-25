import {
  createVirtualizer,
  defaultRangeExtractor,
  elementScroll,
  observeElementRect,
  type Range,
  type VirtualItem,
} from "@tanstack/solid-virtual"
import { isScrollKeyTarget, scrollKey, scrollKeyOwner, ScrollView } from "@opencode/ui/scroll-view"
import { Predicate } from "effect"
import { Timeline, TimelineRow } from "@opencode/session-ui/timeline/projection"
import { useLanguage } from "@/runtime/i18n/language"
import {
  batch,
  createEffect,
  createMemo,
  createSignal,
  For,
  on,
  onCleanup,
  onMount,
  Show,
  type Accessor,
  type JSX,
} from "solid-js"
import { createStore, reconcile } from "solid-js/store"
import { createMediaQuery } from "@solid-primitives/media"
import type { createTimelineProjection } from "./projection"
import { observeElementOffsetReconnectAware } from "./observe-element-offset"
import { filterVirtualIndexes } from "./virtual-items"
import { prefersReducedMotion } from "@opencode/ui/motion"
import { holdLandingWidth } from "@/shell/panel-motion"

const fallbackItemSize = 60

// Reserved below the last row: a 36px lane for the live status row and its background hint, an 8px gap, then a 28px
// activity pill and its 10px composer gap. Both come and go inside it, so neither moves the rows above.
const tailSize = 82

const pendingMarkdown = '[data-component="markdown"]:not([data-markdown-ready])'

// Distance from the bottom that counts as "at the end". Deliberately tight: a collapse clamps
// exactly to the end, while a one-pixel nudge upward is a deliberate move away from it.
const endEpsilon = 0.5

// Chrome rounds scrollHeight and clientHeight to whole pixels but scrolls in zoomed ones, so under page zoom (down to
// 25%) the furthest offset it reaches can sit up to 4px short of scrollHeight - clientHeight.
const zoomSlack = 4

const upwardKeys = new Set(["up", "page-up", "home"])

/** Distance from the top within which an upward gesture asks for an older page. */
export const historyTopThreshold = 200

type RestoreAnchor = { key: string; top: number }

/** Whether a disposed timeline left a mid-history position an unpinned remount would restore. */
export function hasCachedRestore(sessionKey: string) {
  return restorePlan(cache.get(sessionKey), false).restore
}

type TimelineCache = {
  measurements: VirtualItem[]
  toolOpen: Record<string, boolean | undefined>
  patchGroupKeys: Map<string, string>
  presentationKey?: string
  /** listRoot.scrollTop when the view was disposed. */
  offset: number
  /** The view was pinned at its reachable end when it was disposed. */
  atEnd: boolean
  /** The first row crossing the viewport's top edge, so a remount can put it back there. */
  anchor?: RestoreAnchor
}

const cache = new Map<string, TimelineCache>()

export type RestorePlan = { offset: number; restore: boolean }

/**
 * Where a remounting timeline opens. A pinned session keeps following its end; a reader who left
 * mid-history resumes the offset captured at dispose instead of landing on the oldest row.
 */
export function restorePlan(
  cached: Pick<TimelineCache, "offset" | "atEnd" | "anchor"> | undefined,
  pinned: boolean,
): RestorePlan {
  // Missing or at-end state keeps today's behavior: end when pinned, otherwise the initial 0.
  if (pinned) return { offset: Number.MAX_SAFE_INTEGER, restore: false }
  if (!cached || cached.atEnd || !(cached.offset > 0)) return { offset: 0, restore: false }
  return { offset: cached.offset, restore: true }
}

/** Keeps a restored offset inside the range a freshly bound list root reports. */
export function clampRestoredOffset(offset: number, scrollHeight: number, clientHeight: number) {
  return Math.max(0, Math.min(offset, Math.max(0, scrollHeight - clientHeight)))
}

/**
 * The offset a reattached scroll element must be put back to. A cached view is removed from the page
 * and inserted again on a mobile view switch, and an element starts a fresh insertion at 0, so the
 * reader's position is gone unless it is written back. `last` is the offset of the element's last
 * real scroll report; `live` is what the reattached element reports now. Returns `undefined` when no
 * write is needed: a pinned view keeps following its end, a root that already reports the reading
 * offset needs nothing, and a range too short to reach it clamps to the top.
 */
export function reconnectRestore(input: {
  pinned: boolean
  last: number
  live: number
  scrollHeight: number
  clientHeight: number
}) {
  if (input.pinned) return
  if (!(input.last > 0)) return

  const target = clampRestoredOffset(input.last, input.scrollHeight, input.clientHeight)

  if (!(target > 0)) return
  if (Math.abs(target - input.live) <= 1) return

  return target
}

/** The scroll correction that puts a cached anchor row back at the viewport offset it had. */
export function anchorCorrection(anchorTop: number, rowTop: number) {
  const delta = rowTop - anchorTop
  return Math.abs(delta) < 1 ? 0 : delta
}

/**
 * Decides what a first report from a freshly reattached scroll element means. A reattached route
 * reports 0 before layout puts the cached offset back, so that report must not overwrite it.
 * `keep` holds the restored offset; `pending` stays true until a real (non-zero) offset arrives.
 */
export function restoredOffsetReport(pending: boolean, offset: number) {
  if (!pending) return { keep: false, pending: false }
  if (offset <= 0) return { keep: true, pending: true }
  return { keep: false, pending: false }
}

export function prependAnchorDelta(
  prevKeys: readonly string[],
  nextKeys: readonly string[],
  sizeForKey: (key: string) => number,
): { shift: number; prepended: string[]; delta: number } {
  if (prevKeys.length === 0 || nextKeys.length <= prevKeys.length) {
    return { shift: 0, prepended: [], delta: 0 }
  }

  const shift = nextKeys.indexOf(prevKeys[0]!)
  if (shift <= 0) return { shift: 0, prepended: [], delta: 0 }

  if (prevKeys.length > 1) {
    const tailIndex = nextKeys.indexOf(prevKeys[prevKeys.length - 1]!, shift)
    if (tailIndex < shift) return { shift: 0, prepended: [], delta: 0 }
  }

  const prepended = nextKeys.slice(0, shift)
  const delta = prepended.reduce((sum, key) => sum + sizeForKey(key), 0)
  return { shift, prepended, delta }
}

export type LoadHistoryIntent = "user" | "navigation" | "bootstrap" | "restore" | "correction"

/**
 * Whether one more history page may load. Only explicit input may page history: a live
 * wheel/touch/pointer/key gesture (`user`), a targeted hash navigation, or the bounded
 * short-viewport bootstrap. Programmatic restore/anchor/scroll repair is `restore`/`correction`
 * and never loads, so a correction that lands near the top cannot start a prepend -> correction
 * loop that jumps the reader to older messages.
 */
export function shouldLoadHistory(input: {
  intent: LoadHistoryIntent
  loading: boolean
  more: boolean
  pinned: boolean
  scrollTop: number
  /** The initial short-viewport bootstrap already spent its bounded pages. */
  bootstrapSpent: boolean
}): boolean {
  if (input.loading || !input.more) return false
  if (input.intent === "restore" || input.intent === "correction") return false
  if (input.intent === "navigation") return true
  if (input.intent === "bootstrap") return !input.bootstrapSpent
  return !input.pinned && input.scrollTop < historyTopThreshold
}

type Projection = Pick<
  ReturnType<typeof createTimelineProjection>,
  "activeMessageID" | "messageByID" | "messageLastRowIndex" | "messageRowIndex" | "rowByKey" | "rows"
>

type Input = {
  active?: Accessor<boolean>
  sessionKey: Accessor<string>
  presentationKey?: Accessor<string>
  projection: Projection
  showHeader: Accessor<boolean>
  /** True while the timeline follows the newest content. Drives every anchoring decision. */
  pinned: Accessor<boolean>
  scroll: Accessor<{ overflow: boolean; jump: boolean }>
  onResumeScroll: (behavior?: ScrollBehavior) => void
  setScrollRef: (element: HTMLDivElement | undefined) => void
  setContentRef: (element: HTMLDivElement) => void
  /** Height of the composer region floating over the timeline's bottom edge, where the timeline fades out. */
  bottomInset?: Accessor<number>
  setVisibleRef?: (element: HTMLDivElement | undefined) => void
  onScheduleScrollState: (element: HTMLDivElement) => void
  onPin: () => void
  /** `allowShort` lets an explicit gesture stop following even when the timeline cannot scroll. */
  onUnpin: (options?: { allowShort?: boolean }) => void
  onSelectionInteraction: (event: MouseEvent) => void
  onUserScroll: (target?: EventTarget | null) => void
  onHistoryScroll: (intent?: "user") => void
  onLayoutSettled?: () => void
  setCanBootstrap?: (fn: () => boolean) => void
  canRenderImmediately?: (
    row: TimelineRow.TimelineRow,
    disclosure: Readonly<Record<string, boolean | undefined>>,
  ) => boolean
  setRevealMessage?: (fn: (id: string, partID?: string) => void) => void
  setRevealPart?: (fn: (target: string) => boolean) => void
  setScrollToEnd?: (fn: (behavior?: ScrollBehavior) => void) => void
  /** A tool call the route asks this timeline to open on; `done` clears it once revealed. */
  reveal?: { target: () => string | undefined; done: () => void }
}

type ViewProps = {
  header: JSX.Element
  bottomSpacer?: JSX.Element
  deferred: (row: TimelineRow.TimelineRow) => boolean
  renderRow: (row: Accessor<TimelineRow.TimelineRow>, onSizeChange?: () => void) => JSX.Element
}

export function createTimelineVirtualizer(input: Input) {
  const language = useLanguage()
  const active = () => input.active?.() !== false
  const isDesktop = createMediaQuery("(min-width: 768px)")
  const topOffset = () => (input.showHeader() ? 64 : isDesktop() ? 0 : 16)
  const ownerSessionKey = input.sessionKey()
  const entry = cache.get(ownerSessionKey)
  const cached = entry?.presentationKey === input.presentationKey?.() ? entry : undefined
  // A remount (mobile view switch, tab switch, route return) opens at the cached offset instead of 0.
  const plan = restorePlan(cached, input.pinned())
  const initialMeasurements = cached?.measurements
  const coldBottomMount = !initialMeasurements?.length && input.pinned()
  const [listRoot, setListRoot] = createSignal<HTMLDivElement>()
  const [visibleArea, setVisibleArea] = createSignal<HTMLDivElement>()
  const [viewRoot, setViewRoot] = createSignal<HTMLDivElement>()
  const bottomInset = () => input.bottomInset?.() ?? 0
  const [toolOpen, setToolOpen] = createStore<Record<string, boolean | undefined>>(cached?.toolOpen ?? {})
  const patchGroupKeys = cached?.patchGroupKeys ?? new Map<string, string>()
  createEffect(
    on(
      () => input.presentationKey?.(),
      () => setToolOpen(reconcile({})),
      { defer: true },
    ),
  )

  const [rendering, setRendering] = createStore<{
    initialTail: boolean
    scrollAdjustment: number
    /** A revealed tool whose top edge stays put until the user scrolls. */
    anchor?: { key: string; partID: string }
  }>({ initialTail: coldBottomMount, scrollAdjustment: 0 })

  const rows = input.projection.rows
  const rowByKey = input.projection.rowByKey

  const rowKeys = createMemo(() => rows().map(TimelineRow.key), undefined, {
    equals: (previous, next) => previous.length === next.length && previous.every((key, index) => key === next[index]),
  })

  const knownKeys = new Set(rowKeys())
  const addedKeys = new Set<string>()
  const prependedKeys = new Set<string>()
  let previousKeys = rowKeys()
  // Programmatic corrections (pin, measure, history reconcile) move scrollTop
  // without a user intent. Only a real gesture may load older history when the
  // resulting scroll event lands near the top, otherwise a correction becomes
  // a prepend -> key change -> correction loop that jumps to earlier messages.
  let userScrollIntent = false

  const getItemKey = createMemo(() => {
    const keys = rowKeys()
    const prepend = prependAnchorDelta(
      previousKeys,
      keys,
      (key) => virtualizer.itemSizeCache.get(key) ?? fallbackItemSize,
    )
    if (prepend.shift > 0) {
      prepend.prepended.forEach((key) => prependedKeys.add(key))
    }
    previousKeys = keys

    keys
      .filter((key) => !knownKeys.has(key))
      .forEach((key) => {
        knownKeys.add(key)
        addedKeys.add(key)
      })
    for (const key of [...addedKeys]) {
      if (!keys.includes(key)) {
        addedKeys.delete(key)
        prependedKeys.delete(key)
      }
    }

    return (index: number) => keys[index] ?? "removed"
  })

  const rangeExtractor = createMemo(() => {
    const id = input.projection.activeMessageID()
    const active = id ? (input.projection.messageLastRowIndex().get(id) ?? -1) : -1
    const initialTail = rendering.initialTail && input.pinned()
    const anchored = rendering.anchor ? rowKeys().indexOf(rendering.anchor.key) : -1

    return (range: Range) => {
      // Batch a bounded cheap suffix, but stop before unknown/large content.
      // A large tail still mounts alone before estimates expose earlier history.
      const start = Math.max(0, range.startIndex - 2)

      const boundary = initialTail
        ? rows()
            .slice(start, range.count)
            .findLastIndex(
              (row) =>
                !(
                  Predicate.isTagged(row, "AssistantPart") &&
                  row.group.type === "context" &&
                  row.group.refs.length <= 64 &&
                  !toolOpen[`context:${row.group.key}`]
                ) && !input.canRenderImmediately?.(row, toolOpen),
            )
        : -1

      const first = Math.min(range.count - 1, start + boundary + 1)

      const indexes = initialTail
        ? Array.from({ length: range.count - first }, (_, index) => first + index)
        : defaultRangeExtractor({ ...range, overscan: 2 })

      return filterVirtualIndexes(
        [...new Set([...indexes, ...[active, anchored].filter((index) => index >= 0)])].sort((a, b) => a - b),
        range.count,
      )
    }
  })

  const measuredElements = new WeakSet<Element>()
  let touchStart: number | undefined
  let touchTarget: EventTarget | null = null
  let touchNested = false
  let touchScrolling = false
  let touchAdjustment = 0
  let pointerHeld = false
  let maxScroll = 0
  let virtualContent: HTMLDivElement | undefined
  let rowLayer: HTMLDivElement | undefined
  // While a sidebar's spring moves the timeline's edge, rows keep the width they land at, centred on the timeline, so
  // their text wraps once instead of on every frame. The first column in view (turn gaps span the row) keeps its start
  // edge where it was.
  holdLandingWidth(
    () => rowLayer,
    () => {
      const top = listRoot()?.getBoundingClientRect().top ?? 0

      return [
        ...(rowLayer?.querySelectorAll<HTMLElement>(
          '[data-timeline-key] [data-timeline-row]:not([data-timeline-row="TurnGap"])',
        ) ?? []),
      ].find((row) => row.getBoundingClientRect().bottom > top)
    },
  )
  let scrollTop = 0
  // The last position the reader actually held. A reattached element reports 0 before anything
  // restores it, so the reconnect path cannot read the live offset as the position to return to.
  let lastAttachedOffset = 0
  // A remount keeps the offset captured at dispose until the reattach observer reports a real one.
  let restorePending = plan.restore
  let restoredOffset = plan.offset
  let anchorCorrected = false
  let userScrolledSinceMount = false
  // A scroll arrived within the zoom slack of the end and may have reached it.
  let endPending = false
  let reportOffset: ((offset: number, scrolling: boolean) => void) | undefined
  let reportRect: ((rect: { width: number; height: number }) => void) | undefined
  let batchingColdSizes = false

  const [headerHeight, setHeaderHeight] = createSignal(48)

  const virtualizer = createVirtualizer<HTMLDivElement, HTMLDivElement>({
    get count() {
      return rows().length
    },
    getScrollElement: () => listRoot() ?? null,
    // The viewport runs on under the composer region. The virtualizer sees only the part above it, which the
    // inset spacer below the rows keeps consistent with the native scroll range.
    observeElementRect: (instance, callback) => {
      reportRect = (rect) => {
        const root = listRoot()
        batch(() => {
          // The container-responsive header can be 48px or 80px.
          setHeaderHeight(
            root ? parseFloat(getComputedStyle(root).getPropertyValue("--session-header-height")) || 48 : 48,
          )
          callback({ width: rect.width, height: Math.max(0, rect.height - bottomInset()) })
        })
      }

      return observeElementRect(instance, (rect) => {
        if (active()) reportRect?.(rect)
      })
    },
    // Route navigation detaches and reattaches the scroll element, which drops its offset.
    observeElementOffset: (instance, callback) => {
      reportOffset = (offset, scrolling) => {
        if (!active()) return
        // Reattaching a route reports the fresh element's 0 before layout restores the cached
        // offset. Keep the restored position; end the window on the first real report.
        const decision = restoredOffsetReport(restorePending, offset)

        if (decision.keep) {
          const root = listRoot()

          if (root && restoredOffset > 0 && root.scrollTop !== restoredOffset) root.scrollTop = restoredOffset
          return
        }

        if (!decision.pending) restorePending = false
        if (offset > 0) lastAttachedOffset = offset
        // Rows and the sizer use the opposite translation while native touch
        // scrolling keeps its own offset. Range selection uses the logical offset.
        batch(() => {
          const logicalOffset = offset + rendering.scrollAdjustment
          callback(rendering.scrollAdjustment ? Math.max(0, logicalOffset) : offset, scrolling)
          // Reconcile both start boundaries in one native write. Gradually
          // clamping row translations lets the compositor paint between
          // corrections and makes the content oscillate at the top.
          const root = listRoot()

          if (
            rendering.scrollAdjustment !== 0 &&
            root &&
            (logicalOffset <= 0 || offset <= 0 || (touchStart !== undefined && offset <= root.clientHeight))
          )
            flushTouchAdjustment()

          if (!scrolling && touchStart === undefined) finishTouchScroll()

          if (scrolling || touchStart !== undefined || !endPending) return
          endPending = false

          if (root && !input.pinned() && !rendering.anchor && atReachableEnd(root)) input.onPin()
        })
        settleColdBottom()
      }

      return observeElementOffsetReconnectAware(instance, reportOffset, () => {
        if (!active()) return
        virtualContent?.querySelectorAll<HTMLDivElement>("[data-index]").forEach(virtualizer.measureElement)

        if (input.pinned()) virtualizer.scrollToEnd()
        else restoreReattachedOffset()
        settleColdBottom()
      })
    },
    initialOffset: () => plan.offset,
    initialMeasurementsCache: initialMeasurements,
    estimateSize: () => fallbackItemSize,
    // A newly observed element gets a real ResizeObserver box before paint. Reuse
    // its snapshot on attachment, but later explicit measurements must read layout
    // so deferred/rewrapped content cannot keep stale, clipped heights (TanStack/virtual#1183).
    measureElement: (element, entry, instance) => {
      if (!active() || !element.isConnected)
        return (
          instance.itemSizeCache.get(instance.options.getItemKey(instance.indexFromElement(element))) ??
          fallbackItemSize
        )
      const initial = !measuredElements.has(element)
      measuredElements.add(element)
      const box = entry?.borderBoxSize[0]

      if (box) return Math.round(box.blockSize)

      if (initial) {
        const size = instance.itemSizeCache.get(instance.options.getItemKey(instance.indexFromElement(element)))

        if (size !== undefined || coldPending) return size ?? fallbackItemSize
      }

      return element.offsetHeight
    },
    scrollToFn: (offset, options, instance) => {
      if (!active()) return

      if (batchingColdSizes && input.pinned()) return
      setRendering("scrollAdjustment", 0)

      if (virtualContent) virtualContent.style.height = `${instance.getTotalSize()}px`
      elementScroll(offset, options, instance)
    },
    get getItemKey() {
      return getItemKey()
    },
    get anchorTo() {
      return input.pinned() ? "end" : "start"
    },
    get followOnAppend() {
      return input.pinned()
    },
    scrollEndThreshold: 80,
    get scrollMargin() {
      // Empty projections still need the bottom spacer for running status.
      return rows().length > 0 ? topOffset() : 0
    },
    paddingEnd: tailSize,
    // Revealed prompts sit below the solid header and its 16px fade.
    get scrollPaddingStart() {
      return input.showHeader() ? headerHeight() + 16 : 0
    },
    get rangeExtractor() {
      return rangeExtractor()
    },
  })

  const resizeItem = virtualizer.resizeItem
  const pendingSizes = new Map<number, { key: string; size: number }>()
  let resizeScheduled = false
  // Read the whole measurement delivery before committing reactive row sizes.
  // Otherwise each row can render and force layout before the next is measured.
  virtualizer.resizeItem = (index, size) => {
    if (!active()) return
    const row = rows()[index]

    if (!row) return
    const key = TimelineRow.key(row)

    if ((virtualizer.itemSizeCache.get(key) ?? fallbackItemSize) === size) {
      pendingSizes.delete(index)

      return
    }

    pendingSizes.set(index, { key, size })

    if (resizeScheduled) return
    resizeScheduled = true
    queueMicrotask(() => {
      resizeScheduled = false

      if (!pendingSizes.size) return
      const sizes = [...pendingSizes]
      pendingSizes.clear()

      if (!active()) return
      // The hidden pinned mount needs one bottom write after the whole batch,
      // not a layout-forcing scroll adjustment for every measured row.
      batchingColdSizes = coldPending && input.pinned()
      batch(() => {
        sizes.forEach(([index, value]) => {
          const row = rows()[index]

          if (!row || TimelineRow.key(row) !== value.key) return
          resizeItem(index, value.size)

          // TanStack recalculates its range after each resize. Advance the
          // logical fold before deciding whether the next row needs anchoring.
          if (!touchAdjustment) return
          setRendering("scrollAdjustment", (value) => value + touchAdjustment)
          touchAdjustment = 0
          const root = listRoot()

          if (root) reportOffset?.(root.scrollTop, virtualizer.isScrolling)
        })
      })
      batchingColdSizes = false
      pinAnchor()

      if (coldPending) pinColdBottom()
      settleColdBottom()

      if (coldPending) return

      if (!input.pinned()) return
      const root = listRoot()

      // Reopening a settled scroll-to-end operation can fight subsequent keyboard scrolling.
      if (root && !atReachableEnd(root)) virtualizer.scrollToEnd()
    })
  }

  onCleanup(() => {
    pendingSizes.clear()
    prependedKeys.clear()
  })
  virtualizer.shouldAdjustScrollPositionOnItemSizeChange = (item, delta, instance) => {
    // Prepended rows can resize more than once as deferred content mounts. Keep
    // compensating while they remain above the reader's anchor row. Non-prepended
    // added rows must stay strictly above the viewport fold (item.end already includes
    // scrollMargin in @tanstack/virtual-core).
    const first = instance.range?.startIndex
    const key = String(item.key)

    const adjust = prependedKeys.has(key)
      ? item.end <= (instance.scrollOffset ?? 0) + instance.scrollAdjustments + instance.options.scrollMargin + 1
      : addedKeys.has(key)
        ? item.end <= (instance.scrollOffset ?? 0) + instance.scrollAdjustments
        : first !== undefined && item.index < first

    if (!touchScrolling || input.pinned()) return adjust

    // iOS defers native scroll writes until momentum ends. Keep the same visual
    // anchor now, rather than moving rows now and snapping the viewport back later.
    if (adjust) touchAdjustment += delta

    return false
  }

  function finishTouchScroll() {
    touchScrolling = false
    flushTouchAdjustment()
  }

  // Scrolls to a tool call, named by its ID or by the shell ID a backgrounded shell call reports, and expands it and
  // the collapsed context group that hides it. Returns whether the call is in the list.
  function revealPart(target: string) {
    if (!active()) return false

    const matches = (ref: { messageID: string; partID: string }) => {
      const content = Timeline.resolveContent(input.projection.messageByID().get(ref.messageID), ref.partID)

      return (
        content?.type === "tool" &&
        (content.id === target || (content.state.status !== "streaming" && content.state.metadata?.shellID === target))
      )
    }

    const found = rows()
      .flatMap((row, index) =>
        Predicate.isTagged(row, "AssistantPart")
          ? (row.group.type === "part" ? [row.group.ref] : row.group.refs).flatMap((ref) =>
              matches(ref) ? [{ group: row.group, index, partID: ref.partID }] : [],
            )
          : [],
      )
      .at(-1)

    if (!found) return false

    const key = found.group.key

    prepareNavigation()
    // Opening the group and anchoring its row render the tool synchronously, wherever the row is.
    batch(() => {
      setToolOpen(
        found.group.type === "context"
          ? { [`context:${key}`]: true, [`${key}:tool:${found.partID}`]: true }
          : { [key]: true },
      )
      setRendering("anchor", { key: TimelineRow.key(rows()[found.index]!), partID: found.partID })
    })
    // Until its ResizeObserver delivers, the opened row keeps its collapsed size, so the timeline may not scroll yet
    // (and cannot unpin) or ends above the tool. Commit the real size first.
    const opened = virtualContent?.querySelector<HTMLElement>(`[data-index="${found.index}"]`)

    if (opened) resizeItem(found.index, opened.offsetHeight)
    input.onUnpin()
    pinAnchor()

    return true
  }

  // Puts the anchored tool's top edge at its scroll margin, just below the headers that stick above it. Its own
  // growth extends downward, so only size changes at or above its row can move it; each of those calls this again.
  function pinAnchor() {
    const anchor = rendering.anchor
    const root = listRoot()

    if (!anchor || !root) return
    const element = virtualContent?.querySelector<HTMLElement>(`[data-timeline-part-id="${CSS.escape(anchor.partID)}"]`)

    // Following the end is a different position to hold.
    if (!element || !active() || input.pinned()) return releaseAnchor()

    const offset = Math.min(
      root.scrollHeight - root.clientHeight,
      Math.max(
        0,
        root.scrollTop +
          element.getBoundingClientRect().top -
          root.getBoundingClientRect().top -
          parseFloat(getComputedStyle(element).scrollMarginTop),
      ),
    )

    // Hand over every pin, even one the scroll already reaches: TanStack lands exactly on its last target once the
    // scroll is within a pixel of it, which would undo a compensation for a row above that resized since.
    userScrollIntent = false
    virtualizer.scrollToOffset(offset)
  }

  function releaseAnchor() {
    if (rendering.anchor) setRendering("anchor", undefined)
  }

  // After a remount's sizes settle, put the cached first-visible row back where it was. One correction
  // per mount, never while following the end or after the reader has scrolled.
  function restoreAnchor(anchor: RestoreAnchor | undefined) {
    if (anchorCorrected) return

    if (!anchor || plan.restore === false || input.pinned() || userScrolledSinceMount || restorePending) return
    const root = listRoot()

    if (!root) return
    const row = rowLayer?.querySelector<HTMLElement>(`[data-timeline-key="${CSS.escape(anchor.key)}"]`)

    if (!row) return
    anchorCorrected = true
    const rowTop = row.getBoundingClientRect().top - root.getBoundingClientRect().top
    const delta = anchorCorrection(anchor.top, rowTop)

    if (!delta) return
    userScrollIntent = false
    root.scrollTop = clampRestoredOffset(root.scrollTop + delta, root.scrollHeight, root.clientHeight)
  }

  // A cached timeline is not disposed on a mobile view switch: its root is detached and reattached,
  // which resets the element's own offset to 0. Put an unpinned reader back where they were, once per
  // reattachment, without an intent that could page history.
  function restoreReattachedOffset() {
    const root = listRoot()

    if (!root || !active() || rendering.anchor) return
    const target = reconnectRestore({
      pinned: input.pinned(),
      last: lastAttachedOffset,
      live: root.scrollTop,
      scrollHeight: root.scrollHeight,
      clientHeight: root.clientHeight,
    })

    if (target === undefined) return
    userScrollIntent = false
    lastAttachedOffset = target
    restorePending = true
    restoredOffset = target
    root.scrollTop = target
    // Rows land at their real sizes a frame later; re-clamp once they do, so a shorter range cannot
    // leave the view short of the position it recovered. The window is bounded like a remount's, so a
    // range that can never reach the target cannot swallow every later report.
    requestAnimationFrame(() => {
      if (!root.isConnected) return
      const clamped = clampRestoredOffset(target, root.scrollHeight, root.clientHeight)

      if (clamped !== root.scrollTop) root.scrollTop = clamped
      requestAnimationFrame(() => {
        if (!root.isConnected) return
        restorePending = false
      })
    })
  }

  function prepareNavigation() {
    releaseAnchor()

    if (touchStart === undefined) touchScrolling = false
    flushTouchAdjustment()
  }

  function flushTouchAdjustment() {
    const adjustment = rendering.scrollAdjustment
    const root = listRoot()

    if (!adjustment || !root) return
    // Transfer the translation into the native offset in the same paint.
    batch(() => {
      setRendering("scrollAdjustment", 0)

      if (virtualContent) virtualContent.style.height = `${virtualizer.getTotalSize()}px`
      elementScroll(Math.max(0, root.scrollTop + adjustment), {}, virtualizer)
    })
  }

  const virtualItemByKey = createMemo(
    () => new Map(virtualizer.getVirtualItems().map((item) => [item.key, item] as const)),
  )

  const virtualRowKeys = createMemo(() => virtualizer.getVirtualItems().map((item) => String(item.key)))

  createEffect(() => {
    if (!active()) return
    const root = listRoot()

    if (root) input.setScrollRef(root)

    if (virtualContent) input.setContentRef(virtualContent)
    input.setVisibleRef?.(visibleArea())
    queueMicrotask(() => {
      if (!active() || !root?.isConnected) return
      // A detached view can miss its nonzero ResizeObserver delivery. Publish
      // its real viewport before restoring the offset and admitting rows.
      reportRect?.({ width: root.offsetWidth, height: root.offsetHeight })

      // A route can open this timeline on a tool call, e.g. a shell picked from inside a subagent, instead of its end.
      const target = input.reveal?.target()

      if (target && revealPart(target)) input.reveal?.done()
      else if (input.pinned()) virtualizer.scrollToEnd()

      reportOffset?.(root.scrollTop, false)
      settleColdBottom()
      restoreAnchor(cached?.anchor)
    })
    input.setRevealPart?.(revealPart)
    input.setCanBootstrap?.(() => !coldPending && !rendering.initialTail && !pendingSizes.size && !pendingMeasurements())
    input.setRevealMessage?.((id, partID) => {
      if (!active()) return

      const partIndex = partID
        ? rows().findIndex(
            (row) =>
              Predicate.isTagged(row, "AssistantPart") && row.group.type === "part" && row.group.ref.partID === partID,
          )
        : -1

      const promptIndex = rows().findIndex(
        (row) => (Predicate.isTagged(row, "UserMessage") || Predicate.isTagged(row, "PeerMessage")) && row.userMessageID === id,
      )

      const index =
        partIndex >= 0 ? partIndex : promptIndex >= 0 ? promptIndex : input.projection.messageRowIndex().get(id)

      if (index === undefined) return
      prepareNavigation()
      virtualizer.scrollToIndex(index, { align: partIndex >= 0 ? "center" : "start" })
    })
    input.setScrollToEnd?.((behavior) => {
      const root = listRoot()

      if (!active() || !root?.isConnected) return
      prepareNavigation()
      input.onPin()

      // Glide only the final screen. Crossing the whole history smoothly would stream every row through the virtualizer.
      if (behavior === "smooth")
        virtualizer.scrollToOffset(Math.max(root.scrollTop, root.scrollHeight - root.clientHeight * 2))
      virtualizer.scrollToEnd({ behavior })
    })
  })

  createEffect(
    on(
      bottomInset,
      () => {
        const root = listRoot()

        if (active() && root?.isConnected) reportRect?.({ width: root.offsetWidth, height: root.offsetHeight })
      },
      { defer: true },
    ),
  )

  let coldPending = coldBottomMount
  let settleQueued = false
  let contentObserver: MutationObserver | undefined
  let viewportObserver: ResizeObserver | undefined

  // Within the zoom slack only a write finds the end the browser reaches: one that does not move proves the view is
  // already there, and one that does is undone before anything paints.
  function atReachableEnd(root: HTMLDivElement) {
    const top = root.scrollTop
    const distance = root.scrollHeight - root.clientHeight - top

    if (distance <= endEpsilon) return true

    if (distance > zoomSlack) return false
    root.scrollTop = root.scrollHeight

    if (root.scrollTop === top) return true
    root.scrollTop = top

    return false
  }

  // Returns whether the view now rests at its end with that offset reported.
  const pinColdBottom = () => {
    if (!active()) return false
    const root = listRoot()

    if (!input.pinned() || !virtualContent || !root) return false
    // scrollToEnd computes its target from the DOM, not the new size cache.
    virtualContent.style.height = `${virtualizer.getTotalSize()}px`

    if (!atReachableEnd(root)) virtualizer.scrollToEnd()

    // Report after core size adjustments finish so they cannot apply a delta
    // twice. This avoids waiting a frame for the native scroll event.
    if (virtualizer.scrollOffset !== root.scrollTop) reportOffset?.(root.scrollTop, false)

    return true
  }

  const pendingMeasurements = () => {
    const items = virtualizer.getVirtualItems()

    return (
      (rows().length > 0 && items.length === 0) ||
      items.some((item) => !virtualizer.elementsCache.get(item.key)?.isConnected)
    )
  }

  const settleColdBottom = () => {
    if (!active() || !coldPending || settleQueued) return
    settleQueued = true
    queueMicrotask(() => {
      settleQueued = false
      const root = listRoot()

      if (!coldPending || !virtualContent?.isConnected || !root) return

      if (virtualContent.querySelector(pendingMarkdown)) return

      if (!root.clientHeight) return
      // Markdown can finish before ResizeObserver delivers its new box. The
      // normal measureElement path skips reads while scrolling; this gate needs
      // current boxes before expanding the estimated range or revealing it.
      virtualizer.elementsCache.forEach((element) => {
        if (element.isConnected) virtualizer.resizeItem(virtualizer.indexFromElement(element), element.offsetHeight)
      })

      if (pendingSizes.size || pendingMeasurements()) return
      const pinned = input.pinned()

      // A pinned view rests at the end pinColdBottom reaches. Under page zoom that end can be more than a pixel short
      // of scrollHeight - clientHeight, and TanStack keeps the offset it wrote rather than the one reached. Waiting
      // for either distance to close never ends: each pass reported the offset again, which queued the next pass as
      // a microtask, and the renderer froze.
      if (pinned && !pinColdBottom()) return

      // The scroll event must update the range before newly exposed rows can reveal.
      if (
        !pinned &&
        root.scrollHeight > root.clientHeight &&
        Math.abs((virtualizer.scrollOffset ?? 0) - root.scrollTop) > 1
      )
        return

      if (rendering.initialTail) {
        setRendering("initialTail", false)
        settleColdBottom()

        return
      }

      if (pendingSizes.size || pendingMeasurements() || virtualContent.querySelector(pendingMarkdown)) return
      coldPending = false
      contentObserver?.disconnect()
      viewportObserver?.disconnect()
      virtualContent.style.removeProperty("visibility")
      input.onLayoutSettled?.()
    })
  }

  onMount(() => {
    if (!coldPending || !virtualContent) return
    contentObserver = new MutationObserver(settleColdBottom)
    contentObserver.observe(virtualContent, {
      childList: true,
      subtree: true,
      attributes: true,
      attributeFilter: ["data-markdown-ready"],
    })
    viewportObserver = new ResizeObserver(settleColdBottom)
    const root = listRoot()

    if (root) viewportObserver.observe(root)
    settleColdBottom()
  })

  let measuredSessionKey = input.sessionKey()
  createEffect(() => {
    const key = input.sessionKey()

    if (measuredSessionKey !== key) {
      measuredSessionKey = key
      prependedKeys.clear()
      previousKeys = rowKeys()
      virtualizer.measure()
    }
  })

  const bindListRoot = (root: HTMLDivElement) => {
    if (root === listRoot()) return
    // TanStack owns anchoring; browser scroll anchoring would fight its adjustments.
    root.style.overflowAnchor = "none"
    setListRoot(root)

    // A fresh element starts at 0; put a mid-history reader back before the first report can pin.
    if (restorePending && restoredOffset > 0) {
      userScrollIntent = false
      root.scrollTop = clampRestoredOffset(restoredOffset, root.scrollHeight, root.clientHeight)
      requestAnimationFrame(() => {
        if (!root.isConnected) return

        if (!input.pinned()) {
          const clamped = clampRestoredOffset(restoredOffset, root.scrollHeight, root.clientHeight)

          if (clamped !== root.scrollTop) root.scrollTop = clamped
        }

        requestAnimationFrame(() => {
          if (!root.isConnected) return
          restorePending = false
          // Rows have real sizes by now; put the cached first-visible row back where it was.
          restoreAnchor(cached?.anchor)
        })
      })
    }

    scrollTop = root.scrollTop
    if (scrollTop > 0) lastAttachedOffset = scrollTop
    maxScroll = root.scrollHeight - root.clientHeight

    if (active()) input.setScrollRef(root)
    viewportObserver?.observe(root)
    settleColdBottom()
    // The scrollbar thumb sits beside the viewport (either inside ScrollView or portaled into
    // `visibleArea`) and stops its pointerdown from propagating, so listen in the capture phase.
    const scrollView = root.parentElement

    if (!scrollView) return
    scrollView.addEventListener("pointerdown", handleListPointerDown, { capture: true })
    onCleanup(() => scrollView.removeEventListener("pointerdown", handleListPointerDown, { capture: true }))
  }

  createEffect(() => {
    const track = visibleArea()

    if (!track) return
    track.addEventListener("pointerdown", handleListPointerDown, { capture: true })
    onCleanup(() => track.removeEventListener("pointerdown", handleListPointerDown, { capture: true }))
  })

  // Upward input is the one intent geometry cannot recover: nudging up while still a pixel from
  // the end must stop following, even though the resulting position still looks like the end.
  const handleListWheel = (event: WheelEvent & { currentTarget: HTMLDivElement }) => {
    releaseAnchor()
    userScrollIntent = true
    userScrolledSinceMount = true
    input.onUserScroll(event.target)

    if (event.deltaY < 0) {
      // An upward gesture stops following even when the timeline is too short to scroll, so older
      // pages can still page in above the reader.
      input.onUnpin({ allowShort: true })
      const root = listRoot()

      // Ask from the gesture, not from the resulting scroll event: the virtualizer's own
      // corrections also fire scroll events and would otherwise spend the flag before the reader's
      // scroll arrives. The gate inside loadOlder still requires the view to be near the top, so a
      // wheel mid-history is a cheap no-op.
      input.onHistoryScroll("user")
      // At the exact top nothing can scroll, so no scroll event will arrive to clear the flag; a
      // stale flag would let the next programmatic write (hash seek, restore) page history.
      if (root && root.scrollTop <= 0) userScrollIntent = false
    }
  }

  const handleListTouchStart = (event: TouchEvent) => {
    clearTouchTarget()
    releaseAnchor()
    userScrollIntent = true
    userScrolledSinceMount = true
    input.onUserScroll(event.target)
    touchScrolling = true
    touchStart = event.touches[0]?.clientY
    const root = listRoot()
    const nested = event.target instanceof Element ? event.target.closest<HTMLElement>("[data-scrollable]") : null
    touchNested = !!nested && nested !== root && nested.scrollHeight > nested.clientHeight
    // Native touch events keep their original target, even when streaming or
    // virtualization detaches it. Listen there instead of relying on bubbling.
    touchTarget = event.target
    touchTarget?.addEventListener("touchmove", handleListTouchMove, { passive: true })
    touchTarget?.addEventListener("touchend", handleListTouchEnd, { passive: true })
    touchTarget?.addEventListener("touchcancel", handleListTouchEnd, { passive: true })

    if (root) reportOffset?.(root.scrollTop, virtualizer.isScrolling)
  }

  const handleListTouchMove = (event: Event) => {
    if (!(event instanceof TouchEvent)) return
    const current = event.touches[0]?.clientY

    if (current === undefined || touchStart === undefined) return
    const previous = touchStart
    touchStart = current

    // A retained target can outlive its whole session view. Only the active
    // timeline may change the shared follow state; release still cleans up below.
    if (!active()) return

    // Dragging the content downward reveals earlier messages.
    if (current <= previous) return

    // A nested scrollport owns the intent. If it chains into the timeline at a
    // boundary, the resulting native timeline scroll below will unpin instead.
    if (touchNested) return
    input.onUnpin({ allowShort: true })
    const root = listRoot()

    // A drag at the very top scrolls nothing, so the gesture is the only signal that asks for older rows.
    if (root && root.scrollTop < historyTopThreshold) {
      input.onHistoryScroll("user")
      // A drag that cannot scroll also fires no scroll event to spend the flag.
      if (root.scrollTop <= 0) userScrollIntent = false
    }
  }

  const handleListTouchEnd = () => {
    clearTouchTarget()
    touchStart = undefined

    if (!virtualizer.isScrolling) finishTouchScroll()
  }

  function clearTouchTarget() {
    touchTarget?.removeEventListener("touchmove", handleListTouchMove)
    touchTarget?.removeEventListener("touchend", handleListTouchEnd)
    touchTarget?.removeEventListener("touchcancel", handleListTouchEnd)
    touchTarget = null
  }

  onCleanup(clearTouchTarget)

  // Drag-selecting past the edge and dragging the scrollbar both scroll without a wheel or key,
  // so a held pointer is what separates those from the virtualizer's own measurement adjustments.
  const handleListPointerDown = (event: PointerEvent) => {
    releaseAnchor()
    userScrollIntent = true
    userScrolledSinceMount = true
    input.onUserScroll(event.target)
    pointerHeld = true
  }

  const releasePointer = () => {
    pointerHeld = false
  }

  onMount(() => {
    window.addEventListener("pointerup", releasePointer)
    window.addEventListener("pointercancel", releasePointer)
  })
  onCleanup(() => {
    window.removeEventListener("pointerup", releasePointer)
    window.removeEventListener("pointercancel", releasePointer)
  })

  const handleListKeyDown = (event: KeyboardEvent & { currentTarget: HTMLDivElement }) => {
    const key = scrollKey(event)

    if (!key) return

    if (!isScrollKeyTarget(event.target, key)) return

    if (scrollKeyOwner(event.currentTarget, event.target, key) !== event.currentTarget) return
    releaseAnchor()
    userScrollIntent = true
    userScrolledSinceMount = true
    input.onUserScroll(event.currentTarget)

    if (upwardKeys.has(key)) {
      input.onUnpin({ allowShort: true })
      const root = listRoot()

      // At the top the key cannot scroll, so it asks for the older page directly.
      if (root && root.scrollTop < historyTopThreshold) {
        input.onHistoryScroll("user")
        // A key that cannot scroll fires no scroll event to spend the flag.
        if (root.scrollTop <= 0) userScrollIntent = false
      }
    }
  }

  // Following resumes by arriving at the end, either by scrolling there or by content shrinking
  // under a viewport that was already there. Merely resting near the end is not enough, otherwise
  // a later scroll would overwrite an upward intent expressed a pixel short of the bottom.
  const handleListScroll = (event: Event & { currentTarget: HTMLDivElement }) => {
    if (!active()) return
    const root = event.currentTarget
    const previousTop = scrollTop
    const previousMaxScroll = maxScroll
    scrollTop = root.scrollTop
    maxScroll = root.scrollHeight - root.clientHeight
    const atEnd = maxScroll - scrollTop <= endEpsilon
    const arrived = scrollTop > previousTop + endEpsilon || maxScroll < previousMaxScroll

    if (scrollTop > 0) lastAttachedOffset = scrollTop
    if (scrollTop < previousTop) endPending = false

    // An anchor holds its tool even when reaching it lands at the end; only the user's own scroll lets it go.
    if (maxScroll <= 1 || (atEnd && arrived && !rendering.anchor)) input.onPin()
    else if ((pointerHeld || touchScrolling) && scrollTop < previousTop - endEpsilon) input.onUnpin()
    // Probing the zoomed end now would cancel a scroll still in motion, so it waits for the scroll to settle.
    else if (arrived && !rendering.anchor && maxScroll - scrollTop <= zoomSlack) endPending = true
    settleColdBottom()
    input.onScheduleScrollState(root)
    // Only a live gesture, or the intent a wheel/touch/key listener just set, may page history.
    // Every programmatic correction clears the flag, so a restore/anchor write cannot reach loadOlder.
    if (pointerHeld || touchScrolling || userScrollIntent) {
      userScrollIntent = false
      input.onHistoryScroll("user")
    }
  }

  function View(props: ViewProps) {
    function VirtualRow(rowProps: { rowKey: string }) {
      let element: HTMLDivElement
      const initialItem = virtualItemByKey().get(rowProps.rowKey)!
      const initialRow = rowByKey().get(rowProps.rowKey)!
      const item = createMemo(() => virtualItemByKey().get(rowProps.rowKey) ?? initialItem)
      const row = createMemo(() => rowByKey().get(rowProps.rowKey) ?? rows()[item().index] ?? initialRow)
      const [ready, setReady] = createSignal(initialItem.size <= fallbackItemSize || !props.deferred(initialRow))
      let contentMeasureFrame: number | undefined

      onMount(() => virtualizer.measureElement(element))
      // Prepending history changes data-index, not the keyed element's identity.
      // Its observer reads the current index and delivers any actual size change.
      onCleanup(() => {
        if (contentMeasureFrame !== undefined) cancelAnimationFrame(contentMeasureFrame)
        queueMicrotask(() => virtualizer.measureElement(null))
      })

      return (
        <div
          data-timeline-key={rowProps.rowKey}
          style={{
            position: "absolute",
            top: `${item().start - topOffset() - rendering.scrollAdjustment}px`,
            left: "0",
            width: "100%",
            height: `${item().size}px`,
            overflow: "clip",
            "overflow-clip-margin": Predicate.isTagged(row(), "TurnGap") ? undefined : "0.5px",
          }}
        >
          <div
            ref={(value) => {
              element = value

              if (
                (!Predicate.isTagged(row(), "UserMessage") && !Predicate.isTagged(row(), "PeerMessage")) ||
                !addedKeys.has(rowProps.rowKey) ||
                !input.pinned() ||
                coldPending
              )
                return
              // The optimistic row can paint before ResizeObserver corrects the tail estimates.
              // Measure the mounted tail and pin it in this render's microtask instead.
              queueMicrotask(() => {
                if (!input.pinned() || !virtualContent?.isConnected) return
                virtualizer.elementsCache.forEach((item) => {
                  if (item.isConnected) virtualizer.resizeItem(virtualizer.indexFromElement(item), item.offsetHeight)
                })
                virtualizer.resizeItem(item().index, element.offsetHeight)
                virtualContent.style.height = `${virtualizer.getTotalSize()}px`
                virtualizer.scrollToEnd()
              })
            }}
            data-index={item().index}
            style={{ "min-height": ready() ? undefined : `${initialItem.size}px` }}
          >
            {props.renderRow(row, () => {
              setReady(true)

              if (contentMeasureFrame !== undefined) cancelAnimationFrame(contentMeasureFrame)
              contentMeasureFrame = requestAnimationFrame(() => {
                contentMeasureFrame = undefined

                if (active() && element.isConnected) virtualizer.measureElement(element)
              })
            })}
          </div>
        </div>
      )
    }

    return (
      <div
        ref={setViewRoot}
        class="relative w-full h-full min-w-0"
        style={{
          "--session-timeline-inset": `${bottomInset()}px`,
        }}
      >
        <div
          class="absolute bottom-[calc(var(--session-timeline-inset)+32px)] left-1/2 -translate-x-1/2 z-[60] pointer-events-none transition-[opacity,translate]"
          classList={{
            "opacity-100 translate-y-0 duration-150 ease-[cubic-bezier(.2,.8,.2,1)]":
              input.scroll().overflow && input.scroll().jump,
            "opacity-0 translate-y-1 motion-reduce:translate-y-0 duration-[110ms] ease-[cubic-bezier(.4,0,1,1)]":
              !input.scroll().overflow || !input.scroll().jump,
          }}
          inert={!input.scroll().overflow || !input.scroll().jump}
        >
          <button
            type="button"
            aria-label={language.t("session.messages.jumpToLatest")}
            class="pointer-events-auto flex items-center justify-center w-8 h-7 px-2 py-1.5 rounded-lg border-none cursor-pointer text-v2-text-text-base backdrop-blur-[2px] outline-none hover:bg-[linear-gradient(var(--v2-overlay-simple-overlay-hover),var(--v2-overlay-simple-overlay-hover))] active:bg-[linear-gradient(var(--v2-overlay-simple-overlay-pressed),var(--v2-overlay-simple-overlay-pressed))] focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-v2-border-border-focus"
            style={{
              "background-color": "color-mix(in srgb, var(--v2-background-bg-base) 92%, transparent)",
              "box-shadow": "var(--v2-elevation-raised), 0px 2px 8px var(--v2-background-bg-base)",
            }}
            onClick={() => input.onResumeScroll(prefersReducedMotion() ? "auto" : "smooth")}
          >
            <svg width="16" height="16" viewBox="0 0 16 16" fill="none" aria-hidden="true">
              <path
                d="M12.3333 8.66665L8 13L3.66667 8.66665M8 12.6667V2.83332"
                stroke="currentColor"
                stroke-linecap="square"
              />
            </svg>
          </button>
        </div>
        <ScrollView
          data-slot="session-timeline-scroll"
          viewportRef={bindListRoot}
          thumbContainer={visibleArea()}
          thumbHoverTarget={viewRoot()}
          onBeforeScroll={prepareNavigation}
          verticalScrollAdjustment={rendering.scrollAdjustment}
          onWheel={handleListWheel}
          onTouchStart={handleListTouchStart}
          onKeyDown={handleListKeyDown}
          onScroll={handleListScroll}
          onClick={input.onSelectionInteraction}
          class="relative min-w-0 w-full h-full"
          style={{ "--sticky-accordion-top": input.showHeader() ? "var(--session-header-height, 48px)" : "0px" }}
        >
          <Show when={input.showHeader()} fallback={<div aria-hidden="true" class="h-4 md:hidden" />}>
            {props.header}
          </Show>
          <div
            data-timeline-virtual-content
            ref={(element) => {
              virtualContent = element

              if (active()) input.setContentRef(element)
            }}
            style={{
              height: `${virtualizer.getTotalSize() - rendering.scrollAdjustment}px`,
              position: "relative",
              width: "100%",
              // Rows held wider than the timeline mid-flight don't scroll it sideways.
              "overflow-x": "clip",
              visibility: coldBottomMount ? "hidden" : undefined,
            }}
          >
            <div
              ref={rowLayer}
              class="absolute inset-y-0"
              style={{
                left: "calc((100% - var(--panel-hold-width, 100%)) / 2)",
                width: "var(--panel-hold-width, 100%)",
              }}
            >
              <For each={virtualRowKeys()}>{(rowKey) => <VirtualRow rowKey={rowKey} />}</For>
            </div>
            <div
              data-timeline-row="bottom-spacer"
              class="absolute top-0 left-0 w-full"
              style={{
                height: `${tailSize}px`,
                transform: `translateY(${virtualizer.getTotalSize() - tailSize - rendering.scrollAdjustment}px)`,
              }}
            >
              {props.bottomSpacer}
            </div>
          </div>
          <div aria-hidden="true" data-timeline-inset class="h-[var(--session-timeline-inset)] shrink-0" />
        </ScrollView>
        <div
          ref={setVisibleArea}
          data-slot="session-timeline-visible"
          class="pointer-events-none absolute inset-x-0 top-0 bottom-[var(--session-timeline-inset)]"
        />
      </div>
    )
  }

  function captureRestore(root: HTMLDivElement | undefined): { offset: number; atEnd: boolean; anchor?: RestoreAnchor } {
    if (!root) return { offset: 0, atEnd: false }

    const offset = root.scrollTop
    const atEnd = input.pinned() && atReachableEnd(root)
    const rootTop = root.getBoundingClientRect().top
    const first = [...(rowLayer?.querySelectorAll<HTMLElement>("[data-timeline-key]") ?? [])].find(
      (row) => row.getBoundingClientRect().bottom > rootTop,
    )

    if (!first) return { offset, atEnd }

    const key = first.dataset.timelineKey

    if (!key) return { offset, atEnd }

    return { offset, atEnd, anchor: { key, top: first.getBoundingClientRect().top - rootTop } }
  }

  onCleanup(() => {
    // Capture the visible position while the element still has layout, then the size snapshot.
    const restored = captureRestore(listRoot())
    const snapshot = virtualizer.takeSnapshot()

    cache.delete(ownerSessionKey)
    cache.set(ownerSessionKey, {
      measurements: snapshot,
      toolOpen: { ...toolOpen },
      patchGroupKeys,
      presentationKey: input.presentationKey?.(),
      ...restored,
    })

    while (cache.size > 16) cache.delete(cache.keys().next().value!)
    coldPending = false
    contentObserver?.disconnect()
    viewportObserver?.disconnect()

    if (active()) {
      input.setScrollRef(undefined)
      input.setRevealMessage?.(() => {})
      input.setRevealPart?.(() => false)
      input.setScrollToEnd?.(() => {})
    }
  })

  return {
    disclosure: {
      patchGroupKeys,
      value: (key: string) => toolOpen[key],
      set: (key: string, open: boolean) => {
        // Following the end would scroll an opened user message's top away; it stays where it was pressed.
        if (open && key.startsWith("user-expanded:")) input.onUnpin()
        setToolOpen(key, open)
      },
    },
    revealPart,
    View,
  }
}
