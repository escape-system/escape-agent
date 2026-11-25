import { createEffect, createMemo, createSignal, For, on, onCleanup, Show, untrack } from "solid-js"
import { DragDropProvider, PointerSensor } from "@dnd-kit/solid"
import { isSortable, useSortable } from "@dnd-kit/solid/sortable"
import { AutoScroller, Feedback, PointerActivationConstraints } from "@dnd-kit/dom"
import { RestrictToVerticalAxis } from "@dnd-kit/abstract/modifiers"
import { RestrictToElement } from "@dnd-kit/dom/modifiers"
import { arrayMove } from "@/runtime/array-move"
import { Button } from "@opencode/ui/button"
import { Icon } from "@opencode/ui/icon"
import { IconButton } from "@opencode/ui/icon-button"
import { Loader } from "@opencode/ui/loader"
import { Tooltip } from "@opencode/ui/tooltip"
import { prefersReducedMotion } from "@opencode/ui/motion"
import { useLanguage } from "@/runtime/i18n/language"
import type { SessionQueueView } from "./queue"
import { cascade, Grow, queueStagger } from "./spring-height"
import "./queue-panel.css"

type QueueRow = ReturnType<SessionQueueView["rows"]>[number]

// The IDs the strip renders: `shown` keeps a closing row in its old slot, and any live row that the
// transition effect has not mirrored yet (the panel mounted before its queue hydrated, and that effect
// is deferred so it never ran for the mount value) is appended in queue order. Without this union a
// hydrated durable row is invisible while `live()` already reports it.
export function queuePanelRowIDs(shown: readonly { id: string }[], live: readonly string[]) {
  const kept = shown.map((row) => row.id)

  return [...kept, ...live.filter((id) => !kept.includes(id))]
}

// Pullout above the composer listing the prompts queued behind the current
// turn. The panel slides under the composer card (negative margin, opaque
// composer background) so the two read as one attached surface.
export function SessionQueuePanel(props: { queue: SessionQueueView; hidden?: boolean }) {
  const language = useLanguage()
  // A removed prompt stays drawn in its slot while it closes; the last prompts stay while the emptied panel closes.
  const [shown, setShown] = createSignal(props.queue.rows())
  const live = createMemo(() => props.queue.rows().map((row) => row.id))
  // Every live row renders even when `shown` has not mirrored it yet: the panel can mount before its
  // queue hydrates, and a transition effect that only fires on later changes must never leave a
  // durable row invisible while `live()` already reports it. `shown` still owns rows that are leaving.
  const rendered = createMemo(() => queuePanelRowIDs(shown(), live()))
  const appearing = new Set<string>()
  let lastLive = props.queue.rows()
  const open = () => !props.hidden && live().length > 0
  const count = () => (open() ? live().length : shown().length)
  let listRef!: HTMLDivElement

  createEffect(
    on(
      () => props.queue.rows(),
      (next) => {
        const prevLive = lastLive
        lastLive = next
        if (next.length === 0) return

        // An in-place edit or reorder keeps the row count: a row whose ID it replaced swaps in place rather than closing,
        // while a row that was already closing keeps closing until its exit completes.
        const swap = next.length === prevLive.length && prevLive.length > 0
        if (!swap && prevLive.length > 0 && !props.hidden)
          next.forEach((row) => !prevLive.some((item) => item.id === row.id) && appearing.add(row.id))
        const kept = untrack(shown).filter(
          (row) => !swap || next.some((item) => item.id === row.id) || !prevLive.some((item) => item.id === row.id),
        )
        setShown(keepLeaving(kept, next))
      },
      { defer: true },
    ),
  )

  // When the panel opens with prompts already queued, such as after a dock closes, its rows can cascade in.
  let stopCascade: (() => void) | undefined
  createEffect(
    on(
      open,
      (now, was) => {
        stopCascade?.()
        stopCascade = undefined
        const step = queueStagger()
        if (!now || was !== false || step <= 0 || prefersReducedMotion()) return
        queueMicrotask(() => {
          if (!open()) return
          stopCascade = cascade([...listRef.querySelectorAll<HTMLElement>('[data-component="session-queue-row"]')], step)
        })
      },
      { defer: true },
    ),
  )
  onCleanup(() => stopCascade?.())

  return (
    <Grow when={open()} fade class="relative z-0" onClosed={() => setShown(props.queue.rows())}>
      <div
        data-component="session-queue-panel"
        inert={!open()}
        class="relative z-0 -mb-3 rounded-xl bg-v2-background-bg-base px-1.5 pt-1.5 shadow-[inset_0_0_0_0.5px_var(--v2-border-border-base)]"
        // Match the composer overlap so the scroll crop meets the input edge.
        classList={{ "pb-3": count() > 4, "pb-[18px]": count() <= 4 }}
      >
        <Grow when={count() > 3} fade>
          <div class="px-1.5 pt-1 pb-px text-[11px] font-[530] uppercase leading-[var(--line-height-tight)] tracking-[0.05px] text-v2-text-text-muted [font-variant-numeric:tabular-nums]">
            {language.plural("session.queue.count", count())}
          </div>
        </Grow>
        <DragDropProvider
          sensors={(defaults) => [
            ...defaults.filter((sensor) => sensor !== PointerSensor),
            PointerSensor.configure({
              activationConstraints: [new PointerActivationConstraints.Distance({ value: 4 })],
            }),
          ]}
          modifiers={[RestrictToVerticalAxis, RestrictToElement.configure({ element: () => listRef })]}
          plugins={(defaults) => [
            ...defaults.filter((plugin) => plugin !== AutoScroller && plugin !== Feedback),
            AutoScroller.configure({ acceleration: 8, threshold: { x: 0, y: 0.05 } }),
            Feedback.configure({ dropAnimation: null }),
          ]}
          onDragEnd={(event) => {
            const source = event.operation.source

            if (event.canceled || !isSortable(source)) return
            // A queued control row cannot be re-admitted in a new order; the panel disables drag
            // instead, so a pointer gesture can never reach rewrite()'s guard.
            if (!props.queue.reorderable()) return

            if (source.initialIndex === source.index) return
            void props.queue.reorder(arrayMove(live(), source.initialIndex, source.index))
          }}
        >
          {/* Keyed on row IDs so store updates move row elements instead of
              remounting them, which would kill an in-flight drag. */}
          {/* Four 32px rows, four 1px gaps, and half a row hint at more queued prompts. */}
          <div
            ref={listRef}
            data-slot="session-queue-list"
            class="flex flex-col"
            classList={{ "max-h-[148px] overflow-y-auto": count() > 4 }}
            style={{
              "mask-image":
                count() > 4
                  ? "linear-gradient(to bottom, transparent, black 8px, black calc(100% - 12px), transparent)"
                  : undefined,
            }}
          >
            <For each={rendered()}>
              {(id) => (
                <Grow
                  when={!open() || live().includes(id)}
                  appear={appearing.delete(id)}
                  fade
                  onClosed={() => setShown((rows) => rows.filter((row) => row.id !== id))}
                >
                  <SessionQueueRow queue={props.queue} id={id} index={live().indexOf(id)} />
                </Grow>
              )}
            </For>
          </div>
        </DragDropProvider>
      </div>
    </Grow>
  )
}

function keepLeaving<T extends { id: string }>(previous: T[], next: T[]) {
  return previous.reduce(
    (rows, row, index) => {
      if (rows.some((item) => item.id === row.id)) return rows
      const before = previous.slice(0, index).findLast((item) => rows.some((entry) => entry.id === item.id))
      const at = before ? rows.findIndex((item) => item.id === before.id) + 1 : 0

      return [...rows.slice(0, at), row, ...rows.slice(at)]
    },
    [...next],
  )
}

function SessionQueueRow(props: { queue: SessionQueueView; id: string; index: number }) {
  const language = useLanguage()
  const row = createMemo((last?: QueueRow) => props.queue.rows().find((entry) => entry.id === props.id) ?? last)
  const leaving = () => props.index < 0
  const editing = () => props.queue.editing() === props.id
  // While the turn is stopped the queue stays parked, so the first prompt
  // shows its actions without hover and its label reads Send: that is how a
  // parked queue resumes.
  const active = () => !props.queue.working() && props.index === 0

  const sortable = useSortable({
    get id() {
      return props.id
    },
    get index() {
      return props.index
    },
    get disabled() {
      return props.queue.busy() || leaving() || !props.queue.reorderable()
    },
  })

  return (
    <Show when={row()} keyed>
      {(entry) => (
        <div
          ref={sortable.ref}
          data-component={leaving() ? undefined : "session-queue-row"}
          data-leaving={leaving() || undefined}
          aria-busy={leaving() ? undefined : entry.pending}
          class="group/queue-row mb-px flex min-h-11 shrink-0 items-center justify-between gap-2 rounded-md py-1 ps-0.5 pe-2 md:min-h-0 md:h-8"
          classList={{
            "bg-v2-overlay-simple-overlay-hover": editing(),
            "opacity-60": sortable.isDragSource(),
          }}
        >
          <div class="flex min-w-0 flex-1 items-center gap-2">
            <Show
              when={props.queue.reorderable()}
              fallback={
                // Keeps the text column aligned with reorderable rows without offering a gesture
                // the server would refuse.
                <span aria-hidden="true" class="shrink-0 p-1">
                  <span class="block size-[10px]" />
                </span>
              }
            >
              <button
                ref={sortable.handleRef}
                type="button"
                class="grid min-h-11 min-w-11 shrink-0 cursor-grab touch-none grid-cols-2 place-items-center gap-x-[2px] gap-y-[2.25px] p-1 md:min-h-0 md:min-w-0"
                aria-label={language.t("session.queue.reorder")}
              >
                <For each={Array.from({ length: 6 })}>
                  {() => <span class="size-[2px] bg-v2-background-bg-layer-04" />}
                </For>
              </button>
            </Show>
            <div class="flex min-w-0 items-center gap-4">
              <button
                type="button"
                data-action="session-queue-edit"
                dir="auto"
                aria-disabled={entry.pending || entry.synthetic}
                class="max-w-full min-w-0 self-start truncate rounded-sm text-start text-[13px] font-[440] leading-[var(--line-height-compact)]"
                classList={{
                  "text-v2-text-text-faint": editing() || entry.pending,
                  "cursor-progress": entry.pending,
                  "cursor-text text-v2-text-text-base hover:bg-v2-overlay-simple-overlay-hover":
                    !editing() && !entry.pending && !entry.synthetic,
                }}
                onClick={() => {
                  if (entry.synthetic) return
                  props.queue.edit(props.id)
                }}
              >
                {entry.text ||
                  (entry.attachments ? language.plural("session.queue.attachments", entry.attachments) : "")}
              </button>
              <Show when={entry.attachments && entry.text}>
                <span class="shrink-0 whitespace-nowrap text-[13px] font-[440] leading-[var(--line-height-compact)] text-v2-text-text-muted">
                  {language.plural("session.queue.attachments", entry.attachments)}
                </span>
              </Show>
              {/* A queued peer post is read-only: it has no draft to edit or undo, so the strip names its origin. */}
              <Show when={entry.synthetic}>
                <span class="shrink-0 whitespace-nowrap rounded-sm bg-v2-overlay-simple-overlay-hover px-1 text-[11px] font-[530] uppercase leading-4 text-v2-text-text-muted">
                  {language.t("session.peer.badge")}
                </span>
              </Show>
            </div>
          </div>
          <div
            data-slot="session-queue-actions"
            class="flex shrink-0 items-center gap-1.5"
            classList={{
              "opacity-0 focus-within:opacity-100 group-hover/queue-row:opacity-100 [@media(hover:none)]:opacity-100":
                !active() && !editing() && !entry.pending,
              "pointer-events-none": props.queue.busy() || leaving(),
            }}
          >
            <Show when={entry.pending}>
              <Loader class="size-3.5 text-v2-text-text-faint" />
            </Show>
            <Show when={!editing() && !entry.pending && !entry.synthetic}>
              <Tooltip
                placement="top"
                inactive={!props.queue.working()}
                value={language.t("session.queue.steerTooltip")}
              >
                <Button
                  data-action="session-queue-steer"
                  type="button"
                  size="small"
                  variant="ghost-faint"
                  icon="arrow-up"
                  disabled={props.queue.busy()}
                  class="min-h-11 min-w-11 px-2.5 ![font-weight:530] md:min-h-0 md:min-w-0"
                  onClick={() => void props.queue.steer(props.id)}
                >
                  {props.queue.working() ? language.t("session.queue.steer") : language.t("session.queue.send")}
                </Button>
              </Tooltip>
              <Tooltip placement="top" value={language.t("session.queue.undo")}>
                <IconButton
                  data-action="session-queue-undo"
                  type="button"
                  size="small"
                  variant="ghost-muted"
                  icon={<Icon name="outline-undo" />}
                  disabled={props.queue.busy()}
                  class="size-11 md:size-auto"
                  aria-label={language.t("session.queue.undo")}
                  onClick={() => props.queue.undo(props.id)}
                />
              </Tooltip>
            </Show>
            <Show when={!entry.pending}>
              <Tooltip placement="top" value={language.t("session.queue.remove")}>
                <IconButton
                  data-action="session-queue-remove"
                  type="button"
                  size="small"
                  variant="ghost-muted"
                  icon={<Icon name="outline-xmark" />}
                  disabled={props.queue.busy()}
                  class="size-11 md:size-auto"
                  aria-label={language.t("session.queue.remove")}
                  onClick={() => void props.queue.remove(props.id)}
                />
              </Tooltip>
            </Show>
          </div>
        </div>
      )}
    </Show>
  )
}
