import { Button } from "@opencode/ui/button"
import { Icon } from "@opencode/ui/icon"
import { IconButton } from "@opencode/ui/icon-button"
import { Textarea } from "@opencode/ui/textarea"
import { For, Show, createEffect, createSignal, onCleanup } from "solid-js"
import { useLanguage } from "@/runtime/i18n/language"
import { useServer } from "@/runtime/server/current"
import { isConfigObject, serverMessage, useAgenticRequest } from "@/settings/agentic/config"
import type { PeerFeed, PeerRow, PeerThreadRow } from "./agent-events"
import { asText, type PeerMessage } from "./agent-wire"

type Delivery = "steer" | "queue"

const FOCUSABLE =
  'a[href], button:not([disabled]), input:not([disabled]), textarea:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])'

/**
 * Peer-message drawer for a child session (`docs/13-ui.md`).
 *
 * The list is hydrated from the store (`createPeerFeed`): delivered transcript
 * rows, inbox rows still queued, and live `subagent.peer.message` arrivals. The
 * composer posts into the same session through
 * `POST /api/experimental/subagent/:SID/message`; the returned inbox row is
 * folded in by id, so a queued post survives close/reopen. On phones the same
 * surface is a bottom sheet above the visual-viewport keyboard.
 */
export function SessionPeerDrawer(props: { sessionID: string; feed: PeerFeed; open: boolean; onClose: () => void }) {
  const language = useLanguage()
  const server = useServer()
  const request = useAgenticRequest()
  const [draft, setDraft] = createSignal("")
  const [sending, setSending] = createSignal<Delivery>()
  const [busyID, setBusyID] = createSignal<string>()
  const [error, setError] = createSignal<string>()
  let list: HTMLDivElement | undefined
  let composer: HTMLTextAreaElement | undefined
  let panel: HTMLDivElement | undefined
  let opener: HTMLElement | undefined

  // Remember what opened the sheet so Escape and the close button return focus
  // to it instead of dropping the caret on the document body.
  createEffect(() => {
    if (props.open) {
      opener = document.activeElement instanceof HTMLElement ? document.activeElement : undefined
      queueMicrotask(() => composer?.focus())
    }
  })

  const threadItems = (): PeerThreadRow[] =>
    props.feed.thread?.() ??
    props.feed.messages().map((item) => ({ ...item, role: "question" as const, origin: "peer" as const }))

  const deliveredThread = () => threadItems().filter((item) => item.role === "answer" || item.status !== "queued")

  // Opening the drawer (and arriving messages while it stays open) marks them read
  // so the badge clears instead of counting own sends or open-drawer arrivals.
  createEffect(() => {
    const rows = threadItems()
    if (!props.open) return
    props.feed.markRead()
    if (rows.length > 0) queueMicrotask(() => list?.scrollTo({ top: list.scrollHeight }))
  })

  const close = () => {
    props.onClose()
    // The opener may have unmounted with the composer region; focus it only when
    // it is still in the document.
    if (opener?.isConnected) opener.focus()
  }

  const trap = (event: KeyboardEvent) => {
    if (event.key === "Escape") {
      event.preventDefault()
      event.stopPropagation()
      close()

      return
    }
    if (event.key !== "Tab" || !panel) return
    const nodes = [...panel.querySelectorAll<HTMLElement>(FOCUSABLE)].filter((node) => node.offsetParent !== null)
    if (nodes.length === 0) return
    const first = nodes[0]!
    const last = nodes[nodes.length - 1]!
    const active = document.activeElement

    if (!event.shiftKey && active === last) {
      event.preventDefault()
      first.focus()
    } else if (event.shiftKey && (active === first || !panel.contains(active))) {
      event.preventDefault()
      last.focus()
    }
  }

  createEffect(() => {
    if (!props.open) return
    document.addEventListener("keydown", trap, true)
    onCleanup(() => document.removeEventListener("keydown", trap, true))
  })

  const sender = (message: PeerRow) => {
    if (message.own) return language.t("session.peer.you")
    const title = server.ctx.data.session.get(message.from)?.title?.trim()

    return title ?? message.from
  }

  const send = async (delivery: Delivery) => {
    const text = draft().trim()
    if (!text || sending()) return
    setSending(delivery)
    setError(undefined)

    try {
      const response = await request(`/api/experimental/subagent/${encodeURIComponent(props.sessionID)}/message`, {
        method: "POST",
        body: JSON.stringify({ text, delivery }),
      })
      if (!response.ok) {
        setError(await serverMessage(response))

        return
      }

      const body: unknown = await response.json()
      const item = isConfigObject(body) && isConfigObject(body.data) ? body.data : undefined
      // Fold the returned inbox row in by id; the store lists it once the
      // enqueue event lands, and the id keeps the two from duplicating.
      const message: PeerMessage = {
        id: asText(item?.id) ?? `local:${Date.now()}`,
        from: "you",
        own: true,
        text,
        created: Date.now(),
      }
      props.feed.add(message, delivery)
      setDraft("")
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))
    } finally {
      setSending(undefined)
    }
  }

  const runRowAction = async (id: string, fn: ((id: string) => Promise<void>) | undefined) => {
    if (!fn || busyID()) return
    setBusyID(id)
    setError(undefined)
    try {
      await fn(id)
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))
    } finally {
      setBusyID(undefined)
    }
  }

  const answerRow = (answer: Extract<PeerThreadRow, { role: "answer" }>) => (
    <li
      data-peer-answer={answer.id}
      data-peer-role="answer"
      class="mr-auto w-fit max-w-[92%] rounded-lg border border-v2-border-border-base/70 bg-v2-background-bg-layer-01/70 p-2.5"
    >
      <div class="mb-1 flex items-center gap-1.5 text-11-regular text-v2-text-text-muted">
        <span class="rounded-full bg-v2-background-bg-layer-02 px-1.5 py-px font-[530] text-v2-text-text-base">
          {language.t("session.peer.answer")}
        </span>
        <Show when={answer.agent}>
          {(agent) => <span class="capitalize text-v2-text-text-faint">{agent()}</span>}
        </Show>
        <Show when={answer.streaming}>
          <span class="text-v2-text-text-accent">{language.t("session.peer.answerStreaming")}</span>
        </Show>
      </div>
      <Show when={answer.text}>
        <div
          class="whitespace-pre-wrap break-words text-12-regular leading-5"
          classList={{
            "text-v2-state-fg-danger": !!answer.error,
            "text-v2-text-text-base": !answer.error,
          }}
          dir="auto"
        >
          {answer.text}
        </div>
      </Show>
    </li>
  )

  const row = (message: PeerRow & { origin?: "user" | "peer" }) => {
    const rowBusy = () => busyID() === message.id
    return (
      <li
        data-peer-message={message.id}
        data-peer-role="question"
        aria-busy={rowBusy() || undefined}
        class="ml-auto w-fit max-w-[92%] rounded-lg border p-2.5 transition-colors"
        classList={{
          "border-dashed border-v2-border-border-focus/60 bg-v2-background-bg-layer-01/80":
            message.status === "queued",
          "border-v2-border-border-base bg-v2-background-bg-layer-02/70":
            message.status !== "queued" && (message.own || message.origin === "user"),
          "border-v2-border-border-base bg-v2-background-bg-base":
            message.status !== "queued" && !message.own && message.origin !== "user",
          "opacity-60": rowBusy(),
        }}
      >
        <div class="mb-1.5 flex items-center gap-1.5">
          <span
            class="shrink-0 rounded-full px-1.5 py-px text-11-regular"
            classList={{
              "bg-v2-background-bg-layer-02 text-v2-text-text-accent": !message.own && message.origin !== "user",
              "bg-v2-surface-surface-muted text-v2-text-text-muted": message.own || message.origin === "user",
            }}
          >
            {message.origin === "user"
              ? language.t("session.peer.initialPrompt")
              : language.t(message.own ? "session.peer.you" : "session.peer.badge")}
          </span>
          <span class="min-w-0 flex-1 truncate text-11-regular text-v2-text-text-faint" dir="auto">
            {sender(message)}
          </span>
          <Show when={message.origin !== "user" && message.status}>
            {(status) => (
              <span
                data-peer-status={status()}
                class="shrink-0 rounded-full bg-v2-surface-surface-muted px-1.5 py-px text-11-regular"
                classList={{
                  "text-v2-text-text-accent font-medium": status() === "queued",
                  "text-v2-text-text-muted": status() !== "queued",
                }}
              >
                {status() === "queued" && message.delivery === "steer"
                  ? language.t("session.queue.steer")
                  : language.t(status() === "queued" ? "session.peer.queued" : "session.peer.sent")}
              </span>
            )}
          </Show>
          <Show when={message.agent}>
            {(agent) => (
              <span class="shrink-0 text-11-regular capitalize text-v2-text-text-faint">{agent()}</span>
            )}
          </Show>
          <Show when={message.kind}>
            {(kind) => <span class="shrink-0 text-11-regular text-v2-text-text-faint">{kind()}</span>}
          </Show>
        </div>
        <div class="whitespace-pre-wrap break-words text-12-regular leading-5 text-v2-text-text-base" dir="auto">
          {message.text}
        </div>
        <Show when={message.status === "queued" && (props.feed.steer || props.feed.cancel)}>
          <div class="mt-2 flex items-center justify-end gap-1.5 border-t border-v2-border-border-base/60 pt-1.5">
            <Show when={message.delivery !== "steer" && props.feed.steer}>
              {(steerFn) => (
                <Button
                  type="button"
                  variant="ghost-muted"
                  size="small"
                  class="min-h-11 min-w-11 px-3 md:min-h-0 md:min-w-0 md:h-7 md:px-2"
                  disabled={rowBusy()}
                  onClick={() => void runRowAction(message.id, steerFn())}
                >
                  {language.t("session.queue.steer")}
                </Button>
              )}
            </Show>
            <Show when={props.feed.cancel}>
              {(cancelFn) => (
                <Button
                  type="button"
                  variant="ghost-faint"
                  size="small"
                  class="min-h-11 min-w-11 px-3 md:min-h-0 md:min-w-0 md:h-7 md:px-2"
                  disabled={rowBusy()}
                  onClick={() => void runRowAction(message.id, cancelFn())}
                >
                  {language.t("common.cancel")}
                </Button>
              )}
            </Show>
          </div>
        </Show>
      </li>
    )
  }

  return (
    <Show when={props.open}>
      <div
        data-component="session-peer-backdrop"
        class="fixed inset-0 z-[79] bg-black/40 md:hidden"
        aria-hidden="true"
        onClick={close}
      />
      <div
        ref={panel}
        data-component="session-peer-drawer"
        role="dialog"
        aria-modal="true"
        aria-label={language.t("session.peer.title")}
        class="fixed inset-x-0 bottom-[var(--peer-bottom)] [--peer-bottom:var(--keyboard-inset,0px)] md:[--peer-bottom:6rem] z-[80] flex max-h-[min(85dvh,100dvh)] flex-col overflow-hidden rounded-t-xl border border-v2-border-border-base bg-v2-background-bg-base shadow-[0_16px_40px_rgba(0,0,0,0.28)] md:inset-x-auto md:end-4 md:top-16 md:max-h-none md:w-[360px] md:rounded-xl"
        style={{
          "padding-bottom":
            "max(8px, var(--safe-area-inset-bottom, env(safe-area-inset-bottom, 0px)))",
        }}
      >
        <div class="flex h-11 shrink-0 items-center gap-2 border-b border-v2-border-border-base px-3 md:h-9">
          <Icon name="chat-bubble" size="small" class="shrink-0 text-v2-icon-icon-faint" />
          <span class="min-w-0 flex-1 truncate text-12-medium text-v2-text-text-base">
            {language.t("session.peer.title")}
          </span>
          <Show when={props.feed.queued().length > 0}>
            <span
              data-testid="session-peer-queued-count"
              class="shrink-0 rounded-full bg-v2-background-bg-layer-02 px-2 text-11-regular tabular-nums text-v2-text-text-muted"
            >
              {language.t("session.peer.queued")} {props.feed.queued().length}
            </span>
          </Show>
          <IconButton
            icon={<Icon name="outline-xmark" />}
            variant="ghost-muted"
            size="small"
            class="size-11 shrink-0 md:size-auto"
            aria-label={language.t("session.peer.close")}
            onClick={close}
          />
        </div>

        <div ref={list} class="min-h-0 flex-1 overflow-y-auto overscroll-contain px-3 py-2">
          <Show
            when={deliveredThread().length > 0 || props.feed.queued().length > 0}
            fallback={<div class="text-11-regular text-v2-text-text-faint">{language.t("session.peer.empty")}</div>}
          >
            <ol data-peer-section="delivered" class="flex flex-col gap-2">
              <For each={deliveredThread()}>
                {(entry) => (entry.role === "answer" ? answerRow(entry) : row(entry))}
              </For>
            </ol>
            <Show when={props.feed.queued().length > 0}>
              <div class="mt-3 mb-1 text-11-regular font-medium uppercase tracking-wide text-v2-text-text-faint">
                {language.t("session.peer.queuedSection")}
              </div>
              <ol data-peer-section="queued" class="flex flex-col gap-2">
                <For each={props.feed.queued()}>{(message) => row(message)}</For>
              </ol>
            </Show>
          </Show>
        </div>

        <div class="flex shrink-0 flex-col gap-1.5 border-t border-v2-border-border-base p-2">
          <Textarea
            ref={(element: HTMLTextAreaElement) => {
              composer = element
            }}
            rows={2}
            class="w-full"
            value={draft()}
            placeholder={language.t("session.peer.placeholder")}
            aria-label={language.t("session.peer.placeholder")}
            onInput={(event) => setDraft(event.currentTarget.value)}
            onKeyDown={(event) => {
              if (event.key !== "Enter" || event.shiftKey || event.isComposing) return
              event.preventDefault()
              void send("steer")
            }}
          />
          <Show when={error()}>
            {(message) => (
              <div role="alert" class="text-11-regular text-v2-state-fg-danger">
                {language.t("session.peer.error", { message: message() })}
              </div>
            )}
          </Show>
          <div class="flex items-center justify-end gap-1.5">
            <Button
              type="button"
              variant="ghost-muted"
              size="small"
              class="h-11 min-w-11 px-3 md:h-auto md:min-w-0 md:px-2"
              disabled={!!sending() || !draft().trim()}
              onClick={() => void send("queue")}
            >
              {language.t("session.peer.queue")}
            </Button>
            <Button
              type="button"
              variant="submit"
              size="small"
              class="h-11 min-w-11 px-3 md:h-auto md:min-w-0 md:px-2"
              disabled={!!sending() || !draft().trim()}
              data-testid="session-peer-send"
              onClick={() => void send("steer")}
            >
              {sending() === "steer" ? language.t("session.peer.sending") : language.t("session.peer.send")}
            </Button>
          </div>
        </div>
      </div>
    </Show>
  )
}
