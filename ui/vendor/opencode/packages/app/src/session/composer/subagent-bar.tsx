import { Button } from "@opencode/ui/button"
import { Icon } from "@opencode/ui/icon"
import { Keybind } from "@opencode/ui/keybind"
import { Slash } from "@opencode/ui/slash"
import { SessionProgressIndicatorV2 } from "@opencode/session-ui/v2/session-progress-indicator-v2"
import { createEffect, createSignal, For, Match, onCleanup, Show, Switch, type Accessor, type JSX } from "solid-js"
import { useLanguage } from "@/runtime/i18n/language"
import { formatKeybind } from "@/shell/commands/command"
import { formatElapsed } from "./activity-chips"

export type SubagentStatus = "running" | "succeeded" | "failed" | "interrupted"

// A subagent session takes no prompts, so its composer slot is one quiet status line on the timeline's own surface:
// the run's state and duration, the agent, model and variant, and the way back.
export function SubagentBar(props: {
  agent?: string
  model?: { provider?: string; name: string; variant?: string }
  status?: SubagentStatus
  started: number
  ended?: number
  onBack: () => void
  /** The build lane this session's command waits in, when it has one. */
  lane?: JSX.Element
  /** Peer messages for this child session; the count is what arrived while the drawer was closed. */
  peer?: { count: Accessor<number>; onOpen: () => void }
}) {
  const language = useLanguage()
  const [now, setNow] = createSignal(Date.now())

  // Only what is known shows, with one separator between neighbours.
  const parts = () => [
    ...(props.status ? (["status", "elapsed"] as const) : []),
    ...(props.agent ? (["agent"] as const) : []),
    ...(props.model ? (["model"] as const) : []),
  ]

  createEffect(() => {
    if (props.status !== "running") return

    setNow(Date.now())
    const timer = setInterval(() => setNow(Date.now()), 1000)
    onCleanup(() => clearInterval(timer))
  })

  return (
    // The timeline scrolls on underneath, so the line keeps its surface and fades the text passing into it.
    <div
      data-component="subagent-bar"
      class="relative flex h-8 w-full min-w-0 items-center gap-2 bg-v2-background-bg-base ps-3 pe-1.5 text-[12px] font-[440] leading-4 tracking-[-0.04px] whitespace-nowrap tabular-nums text-v2-text-text-faint before:pointer-events-none before:absolute before:inset-x-0 before:bottom-full before:h-4 before:bg-[linear-gradient(to_top,var(--v2-background-bg-base),transparent)]"
    >
      <Show when={props.status}>
        <span class="flex size-4 shrink-0 items-center justify-center text-v2-icon-icon-faint">
          <Switch>
            <Match when={props.status === "running"}>
              <SessionProgressIndicatorV2 />
            </Match>
            <Match when={props.status === "succeeded"}>
              <Icon name="check" size="small" />
            </Match>
            <Match when={props.status === "failed"}>
              <Icon name="circle-xmark" size="small" />
            </Match>
            <Match when={props.status === "interrupted"}>
              <Icon name="outline-xmark" size="small" />
            </Match>
          </Switch>
        </span>
      </Show>
      <div class="flex min-w-0 flex-1 items-center gap-1.5">
        <For each={parts()}>
          {(part, index) => (
            <>
              <Show when={index() > 0}>
                <span aria-hidden="true">·</span>
              </Show>
              <Switch>
                <Match when={part === "status" && props.status}>
                  {(status) => <span class="shrink-0">{language.t(`session.child.status.${status()}`)}</span>}
                </Match>
                <Match when={part === "elapsed"}>
                  <span class="shrink-0">{formatElapsed((props.ended ?? now()) - props.started, language.t)}</span>
                </Match>
                <Match when={part === "agent" && props.agent}>
                  {(agent) => <span class="shrink-0 capitalize">{agent()}</span>}
                </Match>
                <Match when={part === "model" && props.model}>
                  {(model) => (
                    <span data-slot="subagent-model" class="flex min-w-0">
                      <Show when={model().provider}>
                        <span class="shrink-0">{model().provider}</span>
                        <Slash gap={4} />
                        <span class="sr-only">{" / "}</span>
                      </Show>
                      <span class="truncate">{model().name}</span>
                      {/* Like the message metadata: the variant follows the name, dimmer, with no separator. */}
                      <Show when={model().variant !== "default" && model().variant}>
                        {(variant) => (
                          <>
                            <span class="sr-only">{" "}</span>
                            <span class="ms-1 shrink-0 capitalize opacity-[0.66]">{variant()}</span>
                          </>
                        )}
                      </Show>
                    </span>
                  )}
                </Match>
              </Switch>
            </>
          )}
        </For>
      </div>
      <Show when={props.lane}>{props.lane}</Show>
      <Show when={props.peer}>
        {(peer) => (
          <Button
            type="button"
            variant="ghost-faint"
            size="small"
            class="shrink-0 gap-1.5 px-1.5"
            aria-label={language.t("session.peer.open")}
            onClick={() => peer().onOpen()}
          >
            <Icon name="chat-bubble" size="small" />
            <span class="hidden sm:block">{language.t("session.peer.title")}</span>
            <Show when={peer().count() > 0}>
              <span
                data-testid="session-peer-unread"
                class="rounded-full bg-v2-background-bg-layer-02 px-1.5 text-11-regular tabular-nums text-v2-text-text-accent"
              >
                {language.t("session.peer.unread", { count: String(peer().count()) })}
              </span>
            </Show>
          </Button>
        )}
      </Show>
      <Button type="button" variant="ghost-faint" size="small" class="shrink-0 gap-1.5 px-1.5" onClick={props.onBack}>
        {language.t("session.child.backToParent")}
        <span class="hidden sm:block">
          <Keybind keys={[formatKeybind("esc", language.t)]} variant="neutral" />
        </span>
      </Button>
    </div>
  )
}
