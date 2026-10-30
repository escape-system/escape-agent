import { Icon } from "@opencode/ui/icon"
import { For, Show, createEffect, createMemo, createSignal, onCleanup } from "solid-js"
import { useLanguage } from "@/runtime/i18n/language"
import { useServer } from "@/runtime/server/current"
import { formatElapsed } from "@/session/composer/activity-chips"
import { useOpenSessionRoute } from "@/session/session-identity-header"
import { useSubagentTreeQuery } from "./agent-queries"
import type { SubagentNode } from "./agent-wire"

type Relation = "current" | "parent" | "ancestor" | "sibling" | "child" | "descendant"

/**
 * The session tree above the timeline (`docs/13-ui.md`): parent, children and
 * siblings with the agent, model, depth, state and elapsed time. A row opens
 * that session in the current tab; the routed session is marked and is not a
 * link to itself.
 */
export function SubagentTreePanel(props: { sessionID: string }) {
  const language = useLanguage()
  const server = useServer()
  const open = useOpenSessionRoute()
  const tree = useSubagentTreeQuery(() => props.sessionID)
  const [collapsed, setCollapsed] = createSignal(false)
  const [now, setNow] = createSignal(Date.now())

  const nodes = createMemo(() => (tree.data?.available ? tree.data.value.nodes : []))
  const byID = createMemo(() => new Map(nodes().map((node) => [node.id, node])))

  // Ancestors of the routed session, from the same tree, so the panel can label
  // each row without a second request.
  const ancestors = createMemo(() => {
    const map = byID()
    const path = new Set<string>()
    let id = server.ctx.data.session.get(props.sessionID)?.parentID
    for (let depth = 0; id && depth < 64 && !path.has(id); depth++) {
      path.add(id)
      id = map.get(id)?.parentID
    }

    return path
  })

  const relation = (node: SubagentNode): Relation => {
    if (node.id === props.sessionID) return "current"
    if (ancestors().has(node.id)) return node.parentID === props.sessionID ? "parent" : "ancestor"
    if (node.parentID === props.sessionID) return "child"

    const parent = server.ctx.data.session.get(props.sessionID)?.parentID
    if (parent && node.parentID === parent) return "sibling"

    return "descendant"
  }

  // Only a running tree ticks; a settled tree keeps its final durations.
  createEffect(() => {
    if (!nodes().some((node) => node.state === "busy")) return
    setNow(Date.now())
    const timer = setInterval(() => setNow(Date.now()), 1000)
    onCleanup(() => clearInterval(timer))
  })

  const elapsed = (node: SubagentNode) => {
    const info = server.ctx.data.session.get(node.id)
    if (!info) return undefined
    const end = node.state === "busy" ? now() : (info.time.idle ?? info.time.updated)

    return Math.max(0, end - info.time.created)
  }

  const title = (node: SubagentNode) => {
    const info = server.ctx.data.session.get(node.id)

    return info?.title?.trim() || node.id
  }

  const elapsedLabel = (ms: number) => formatElapsed(ms, (key, params) => language.t(key, params) ?? key)

  return (
    <Show when={tree.data?.available && nodes().length > 1}>
      <div data-component="session-agent-tree" class="border-b border-v2-border-border-base bg-v2-background-bg-base">
        <button
          type="button"
          class="flex w-full items-center gap-2 px-4 py-1.5 text-left text-11-regular text-v2-text-text-muted hover:bg-v2-overlay-simple-overlay-hover md:px-6"
          aria-expanded={!collapsed()}
          aria-label={language.t(collapsed() ? "session.agentPanels.tree.expand" : "session.agentPanels.tree.collapse")}
          onClick={() => setCollapsed((value) => !value)}
        >
          <Icon name="branch-out" size="small" class="shrink-0 text-v2-icon-icon-faint" />
          <span class="shrink-0">{language.t("session.agentPanels.tree.title")}</span>
          <span class="shrink-0 tabular-nums text-v2-text-text-faint">
            {language.t("session.agentPanels.tree.count", { count: String(nodes().length) })}
          </span>
          <span class="flex-1" />
          <Icon name={collapsed() ? "outline-chevron-right" : "chevron-down"} size="small" class="shrink-0" />
        </button>
        <Show when={!collapsed()}>
          <div role="tree" class="pb-1" data-testid="session-agent-tree-nodes">
            <For each={nodes()}>
              {(node) => {
                const kind = relation(node)
                const current = () => kind === "current"

                return (
                  <button
                    type="button"
                    role="treeitem"
                    aria-current={current() ? "true" : undefined}
                    aria-selected={current() ? "true" : undefined}
                    data-session-id={node.id}
                    data-relation={kind}
                    disabled={current()}
                    title={language.t("session.agentPanels.tree.open", { title: title(node) })}
                    class="flex h-7 w-full items-center gap-2 pe-4 text-left text-12-regular hover:bg-v2-overlay-simple-overlay-hover disabled:opacity-100 disabled:hover:bg-transparent"
                    classList={{
                      "font-[530] text-v2-text-text-base": current(),
                      "text-v2-text-text-muted": !current(),
                    }}
                    style={{ "padding-inline-start": `${16 + node.depth * 14}px` }}
                    onClick={() => {
                      if (!current()) open(props.sessionID, node.id)
                    }}
                  >
                    <span class="w-14 shrink-0 text-11-regular text-v2-text-text-faint">
                      {language.t(`session.agentPanels.relation.${kind}`)}
                    </span>
                    <Show when={node.depth > 0}>
                      <span class="shrink-0 text-11-regular tabular-nums text-v2-text-text-faint">
                        {language.t("session.agentPanels.tree.depth", { depth: String(node.depth) })}
                      </span>
                    </Show>
                    <span class="min-w-0 flex-1 truncate" dir="auto">
                      {title(node)}
                    </span>
                    <span class="hidden shrink-0 text-11-regular capitalize text-v2-text-text-faint sm:block">
                      {node.agent}
                    </span>
                    <Show when={node.model}>
                      {(model) => (
                        <span class="hidden max-w-[180px] shrink-0 truncate text-11-regular text-v2-text-text-faint md:block">
                          {model()}
                        </span>
                      )}
                    </Show>
                    <span
                      class="w-16 shrink-0 text-11-regular"
                      classList={{
                        "text-v2-state-fg-success": node.state === "busy",
                        "text-v2-state-fg-danger": node.state === "failed" || node.state === "interrupted",
                        "text-v2-state-fg-warning": node.state === "suspended",
                        "text-v2-text-text-faint":
                          node.state !== "busy" &&
                          node.state !== "failed" &&
                          node.state !== "interrupted" &&
                          node.state !== "suspended",
                      }}
                    >
                      {language.t(`session.agentPanels.state.${node.state}`)}
                    </span>
                    <Show when={elapsed(node)}>
                      {(ms) => (
                        <span class="w-14 shrink-0 text-end text-11-regular tabular-nums text-v2-text-text-faint">
                          {elapsedLabel(ms())}
                        </span>
                      )}
                    </Show>
                  </button>
                )
              }}
            </For>
          </div>
        </Show>
      </div>
    </Show>
  )
}
