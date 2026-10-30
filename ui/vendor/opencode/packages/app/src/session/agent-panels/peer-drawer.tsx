import { Button } from "@opencode/ui/button"
import { Icon } from "@opencode/ui/icon"
import { IconButton } from "@opencode/ui/icon-button"
import { Textarea } from "@opencode/ui/textarea"
import { For, Show, createEffect, createSignal } from "solid-js"
import { useLanguage } from "@/runtime/i18n/language"
import { useServer } from "@/runtime/server/current"
import { isConfigObject, serverMessage, useAgenticRequest } from "@/settings/agentic/config"
import type { PeerFeed } from "./agent-events"
import { asText, type PeerMessage } from "./agent-wire"

type Delivery = "steer" | "queue"

/**
 * Peer-message drawer for a child session (`docs/13-ui.md`).
 *
 * The list is fed by `subagent.peer.message` events whose target is this
 * session; the composer posts into the same session through
 * `POST /api/experimental/subagent/:SID/message` and folds the returned inbox
 * row into the list. Peer messages are not user messages, so each row carries
 * a peer badge and the drawer never touches the transcript.
 */
export function SessionPeerDrawer(props: { sessionID: string; feed: PeerFeed; open: boolean; onClose: () => void }) {
  const language = useLanguage()
  const server = useServer()
  const request = useAgenticRequest()
  const [draft, setDraft] = createSignal("")
  const [sending, setSending] = createSignal<Delivery>()
  const [error, setError] = createSignal<string>()
  let list: HTMLDivElement | undefined

  // Opening the drawer is what reads the messages; the bar's badge counts the
  // ones that arrived while it was closed.
  createEffect(() => {
    if (props.open) props.feed.markRead()
  })

  createEffect(() => {
    const messages = props.feed.messages()
    if (!props.open || messages.length === 0) return
    queueMicrotask(() => list?.scrollTo({ top: list.scrollHeight }))
  })

  const sender = (message: PeerMessage) => {
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
      props.feed.add({
        id: asText(item?.id) ?? `local:${Date.now()}`,
        from: "you",
        own: true,
        text,
        created: Date.now(),
      })
      setDraft("")
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))
    } finally {
      setSending(undefined)
    }
  }

  return (
    <Show when={props.open}>
      <div
        data-component="session-peer-drawer"
        role="dialog"
        aria-label={language.t("session.peer.title")}
        class="fixed bottom-24 end-4 top-16 z-50 flex w-[360px] max-w-[calc(100vw-2rem)] flex-col overflow-hidden rounded-xl border border-v2-border-border-base bg-v2-background-bg-base shadow-[0_16px_40px_rgba(0,0,0,0.28)]"
      >
        <div class="flex h-9 shrink-0 items-center gap-2 border-b border-v2-border-border-base px-3">
          <Icon name="chat-bubble" size="small" class="shrink-0 text-v2-icon-icon-faint" />
          <span class="text-12-medium text-v2-text-text-base">{language.t("session.peer.title")}</span>
          <span class="flex-1" />
          <IconButton
            icon={<Icon name="outline-xmark" />}
            variant="ghost-muted"
            size="small"
            aria-label={language.t("session.peer.close")}
            onClick={props.onClose}
          />
        </div>

        <div ref={list} class="flex-1 min-h-0 overflow-y-auto overscroll-contain px-3 py-2">
          <Show
            when={props.feed.messages().length > 0}
            fallback={<div class="text-11-regular text-v2-text-text-faint">{language.t("session.peer.empty")}</div>}
          >
            <ol class="flex flex-col gap-2">
              <For each={props.feed.messages()}>
                {(message) => (
                  <li data-peer-message={message.id} class="rounded-lg border border-v2-border-border-base p-2">
                    <div class="mb-1 flex items-center gap-1.5">
                      <span
                        class="shrink-0 rounded-full px-1.5 py-px text-11-regular"
                        classList={{
                          "bg-v2-background-bg-layer-02 text-v2-text-text-accent": !message.own,
                          "text-v2-text-text-faint": message.own,
                        }}
                      >
                        {language.t(message.own ? "session.peer.you" : "session.peer.badge")}
                      </span>
                      <span class="min-w-0 flex-1 truncate text-11-regular text-v2-text-text-faint" dir="auto">
                        {sender(message)}
                      </span>
                      <Show when={message.agent}>
                        {(agent) => (
                          <span class="shrink-0 text-11-regular capitalize text-v2-text-text-faint">{agent()}</span>
                        )}
                      </Show>
                      <Show when={message.kind}>
                        {(kind) => <span class="shrink-0 text-11-regular text-v2-text-text-faint">{kind()}</span>}
                      </Show>
                    </div>
                    <div class="whitespace-pre-wrap break-words text-12-regular text-v2-text-text-base" dir="auto">
                      {message.text}
                    </div>
                  </li>
                )}
              </For>
            </ol>
          </Show>
        </div>

        <div class="flex shrink-0 flex-col gap-1.5 border-t border-v2-border-border-base p-2">
          <Textarea
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
              disabled={!!sending() || !draft().trim()}
              onClick={() => void send("queue")}
            >
              {language.t("session.peer.queue")}
            </Button>
            <Button
              type="button"
              variant="submit"
              size="small"
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
