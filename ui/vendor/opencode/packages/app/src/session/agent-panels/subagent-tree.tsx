import { Icon } from "@opencode/ui/icon"
import { For, Show, createEffect, createMemo, createSignal, onCleanup } from "solid-js"
import { useLanguage } from "@/runtime/i18n/language"
import { useServer } from "@/runtime/server/current"
import { formatElapsed } from "@/session/composer/activity-chips"
import { useOpenSessionRoute } from "@/session/session-identity-header"
import { useSubagentTreeQuery } from "./agent-queries"
import type { SubagentNode } from "./agent-wire"
import {
  readSubagentTreeCollapsed,
  subagentCount,
  subagentCountIn,
  subagentTreeNotice,
  writeSubagentTreeCollapsed,
} from "./subagent-tree-state"

type Relation = "current" | "parent" | "ancestor" | "sibling" | "child" | "descendant"
type FilterMode = "all" | "busy" | "failed" | "completed"

function shortModelName(model: string): string {
  const afterSlash = model.includes("/") ? model.split("/").slice(1).join("/") : model
  return afterSlash.replace(/^antigravity-/, "")
}

/**
 * The session tree above the timeline (`docs/13-ui.md`): parent, children and
 * siblings with the agent, model, depth, state and elapsed time. A row opens
 * that session in the current tab; the routed session is marked and is not a
 * link to itself.
 */
export function SubagentTreePanel(props: { sessionID: string; inline?: boolean }) {
  const language = useLanguage()
  const server = useServer()
  const open = useOpenSessionRoute()
  const tree = useSubagentTreeQuery(() => props.sessionID)

  // The desktop header is the only surface that lists every subagent, so the strip starts open
  // there. An explicit collapse is remembered; a stored collapse from the previous key version is
  // upgraded into the new one, so a user who closed it stays closed.
  const [collapsed, setCollapsed] = createSignal(readSubagentTreeCollapsed())
  const [filter, setFilter] = createSignal<FilterMode>("all")
  const [search, setSearch] = createSignal("")
  const [expandedBranches, setExpandedBranches] = createSignal<Record<string, boolean>>({})
  const [allBranchesExpanded, setAllBranchesExpanded] = createSignal(false)

  const toggleCollapsed = () => {
    setCollapsed((val) => {
      const next = !val
      writeSubagentTreeCollapsed(next)
      return next
    })
  }

  const [now, setNow] = createSignal(Date.now())

  // Rows still show the whole tree, including the routed session marked "current". The counts do
  // not: the family root is the routed session itself, not a subagent. The mobile sheet reads the
  // same helpers, so both surfaces report the same total.
  const nodes = createMemo(() => (tree.data?.available ? tree.data.value.nodes : []))
  const rootID = createMemo(() => (tree.data?.available ? tree.data.value.root : undefined))
  const byID = createMemo(() => new Map(nodes().map((node) => [node.id, node])))
  const count = createMemo(() => subagentCount(nodes(), rootID()))
  // A refused read (401/403/503) or a failed one explains itself instead of rendering nothing.
  const notice = createMemo(() => subagentTreeNotice({ isError: tree.isError, error: tree.error, data: tree.data }))

  // Direct children count and total descendant count for each branch node.
  const childCountByParent = createMemo(() => {
    const counts = new Map<string, number>()
    for (const node of nodes()) {
      if (node.parentID) {
        counts.set(node.parentID, (counts.get(node.parentID) ?? 0) + 1)
      }
    }
    return counts
  })

  const hasGrandchildren = createMemo(() => nodes().some((n) => n.depth >= 2))

  // Ancestors of the routed session, from the same tree, so the panel can label
  // each row without a second request.
  const ancestors = createMemo(() => {
    const map = byID()
    const path = new Set<string>()
    let id = server.ctx.data.session.get(props.sessionID)?.parentID ?? map.get(props.sessionID)?.parentID
    for (let depth = 0; id && depth < 64 && !path.has(id); depth++) {
      path.add(id)
      id = map.get(id)?.parentID
    }

    return path
  })

  // A branch at depth >= 1 (which owns depth >= 2 grandchildren) starts collapsed
  // by default unless it is on the active session's ancestor path or explicitly opened.
  const isBranchExpanded = (branchId: string) => {
    const explicit = expandedBranches()[branchId]
    if (explicit !== undefined) return explicit
    if (allBranchesExpanded()) return true
    return ancestors().has(branchId) || branchId === props.sessionID
  }

  const toggleBranch = (branchId: string) => {
    const current = isBranchExpanded(branchId)
    setExpandedBranches((prev) => ({ ...prev, [branchId]: !current }))
  }

  const toggleAllBranches = () => {
    const next = !allBranchesExpanded()
    setAllBranchesExpanded(next)
    setExpandedBranches({})
  }

  const isVisibleInTree = (node: SubagentNode) => {
    if (node.depth <= 1) return true
    const map = byID()
    let parentId = node.parentID
    for (let step = 0; parentId && step < 64; step++) {
      const parent = map.get(parentId)
      if (!parent || parent.depth === 0) break
      if (!isBranchExpanded(parent.id)) return false
      parentId = parent.parentID
    }
    return true
  }

  const relation = (node: SubagentNode): Relation => {
    if (node.id === props.sessionID) return "current"
    if (ancestors().has(node.id)) return node.parentID === props.sessionID ? "parent" : "ancestor"
    if (node.parentID === props.sessionID) return "child"

    const parent = server.ctx.data.session.get(props.sessionID)?.parentID
    if (parent && node.parentID === parent) return "sibling"

    return "descendant"
  }

  // Only tick elapsed durations while the tree panel is open and has active nodes.
  createEffect(() => {
    if (collapsed() || !nodes().some((node) => node.state === "busy")) return
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
    const raw = info?.title?.trim()
    if (raw && raw !== node.id) return raw
    const shortId = node.id.startsWith("ses_") ? node.id.slice(4, 12) : node.id.slice(0, 8)
    return node.agent ? `${node.agent} · ${shortId}` : node.id
  }

  const elapsedLabel = (ms: number) => formatElapsed(ms, (key, params) => language.t(key, params) ?? key)
  const busyCount = createMemo(() => subagentCountIn(nodes(), ["busy"], rootID()))
  const failedCount = createMemo(() => subagentCountIn(nodes(), ["failed", "interrupted"], rootID()))
  const completedCount = createMemo(() => subagentCountIn(nodes(), ["completed", "idle"], rootID()))

  const filteredNodes = createMemo(() => {
    const mode = filter()
    const q = search().trim().toLowerCase()
    const filtering = mode !== "all" || q.length > 0
    return nodes().filter((node) => {
      if (!filtering && !isVisibleInTree(node)) return false
      if (mode === "busy" && node.state !== "busy") return false
      if (mode === "failed" && node.state !== "failed" && node.state !== "interrupted") return false
      if (mode === "completed" && node.state !== "completed" && node.state !== "idle") return false
      if (!q) return true
      const t = title(node).toLowerCase()
      const a = (node.agent ?? "").toLowerCase()
      const m = (node.model ?? "").toLowerCase()
      return t.includes(q) || a.includes(q) || m.includes(q) || node.id.toLowerCase().includes(q)
    })
  })

  // Key `<For>` by primitive session ID strings so the 2-second background poll
  // updates rows in-place instead of recreating DOM nodes and resetting scrollTop.
  const filteredIds = createMemo<string[]>(
    () => filteredNodes().map((n) => n.id),
    [],
    { equals: (a, b) => a.length === b.length && a.every((v, i) => v === b[i]) },
  )

  let treeScrollRef: HTMLDivElement | undefined
  const stopTreeScrollIfOverflowing = (e: Event) => {
    if (treeScrollRef && treeScrollRef.scrollHeight > treeScrollRef.clientHeight) {
      e.stopPropagation()
    }
  }

  // A refused or failed read used to render nothing at all, which is indistinguishable from "this
  // session has no subagents". One short line names the cause instead.
  const unavailable = createMemo(() => (tree.data?.available ? undefined : notice()))

  return (
    <>
      <Show when={unavailable()}>
        {(reason) => (
          <div
            data-component="session-agent-tree-error"
            data-detail={reason().detail}
            role="status"
            class="flex items-center gap-2 border-b border-v2-border-border-base bg-v2-background-bg-base px-4 py-1.5 text-11-regular text-v2-text-text-faint md:px-6"
          >
            <Icon name="circle-exclamation" size="small" class="shrink-0 text-v2-state-fg-warning" />
            <span class="min-w-0 flex-1 truncate">{language.t("session.agentPanels.tree.unavailable")}</span>
          </div>
        )}
      </Show>
      <Show when={tree.data?.available && nodes().length > 1}>
        <div
          data-component="session-agent-tree"
          class="relative border-b border-v2-border-border-base bg-v2-background-bg-base"
        >
          <button
            type="button"
            class="flex w-full items-center gap-2 px-4 py-1.5 text-left text-11-regular text-v2-text-text-muted transition-colors hover:bg-v2-overlay-simple-overlay-hover md:px-6"
            aria-expanded={!collapsed()}
            aria-label={language.t(collapsed() ? "session.agentPanels.tree.expand" : "session.agentPanels.tree.collapse")}
            onClick={toggleCollapsed}
          >
            <Icon name="branch-out" size="small" class="shrink-0 text-v2-icon-icon-faint" />
            <span class="shrink-0 font-medium text-v2-text-text-base">{language.t("session.agentPanels.tree.title")}</span>
            <span class="shrink-0 rounded-full bg-v2-surface-surface-muted px-2 py-0.5 text-[11px] tabular-nums text-v2-text-text-muted">
              {language.t("session.agentPanels.tree.count", { count: String(count()) })}
            </span>
            <Show when={busyCount() > 0}>
              <span class="inline-flex shrink-0 items-center gap-1.5 rounded-full bg-v2-surface-surface-muted px-2 py-0.5 text-[11px] font-medium tabular-nums text-v2-state-fg-success">
                <span class="size-1.5 rounded-full bg-current animate-pulse" />
                {busyCount()} {language.t("session.agentPanels.state.busy")}
              </span>
            </Show>
            <Show when={failedCount() > 0}>
              <span class="inline-flex shrink-0 items-center gap-1 rounded-full bg-v2-surface-surface-muted px-2 py-0.5 text-[11px] font-medium tabular-nums text-v2-state-fg-danger">
                <span class="size-1.5 rounded-full bg-current" />
                {failedCount()} {language.t("session.agentPanels.state.failed")}
              </span>
            </Show>
            <Show when={busyCount() === 0 && completedCount() > 0}>
              <span class="hidden shrink-0 tabular-nums text-11-regular text-v2-text-text-faint sm:inline">
                · {completedCount()} {language.t("session.agentPanels.state.completed")}
              </span>
            </Show>
            <span class="flex-1" />
            <Icon name={collapsed() ? "outline-chevron-right" : "chevron-down"} size="small" class="shrink-0" />
          </button>

          <Show when={!collapsed()}>
            <div
              class="flex flex-col border-b border-v2-border-border-base bg-v2-background-bg-base"
              classList={{
                "absolute inset-x-0 top-full z-40 shadow-[0_12px_32px_rgba(0,0,0,0.35)]": !props.inline,
              }}
              onWheel={stopTreeScrollIfOverflowing}
              onTouchMove={stopTreeScrollIfOverflowing}
            >
              <Show when={nodes().length > 5}>
                <div class="flex flex-wrap items-center justify-between gap-2 border-b border-v2-border-border-base/60 bg-v2-background-bg-layer-01 px-4 py-1.5 md:px-6">
                  <div class="flex flex-wrap items-center gap-1">
                    <button
                      type="button"
                      class="rounded-full px-2.5 py-0.5 text-11-regular tabular-nums transition-colors"
                      classList={{
                        "bg-v2-overlay-simple-overlay-active font-medium text-v2-text-text-base": filter() === "all",
                        "text-v2-text-text-muted hover:bg-v2-overlay-simple-overlay-hover": filter() !== "all",
                      }}
                      onClick={() => setFilter("all")}
                    >
                      {language.t("session.agentPanels.tree.filterAll")} ({count()})
                    </button>
                    <Show when={busyCount() > 0 || filter() === "busy"}>
                      <button
                        type="button"
                        class="inline-flex items-center gap-1 rounded-full px-2.5 py-0.5 text-11-regular tabular-nums transition-colors"
                        classList={{
                          "bg-v2-overlay-simple-overlay-active font-medium text-v2-state-fg-success":
                            filter() === "busy",
                          "text-v2-state-fg-success/80 hover:bg-v2-overlay-simple-overlay-hover": filter() !== "busy",
                        }}
                        onClick={() => setFilter(filter() === "busy" ? "all" : "busy")}
                      >
                        <span class="size-1.5 rounded-full bg-current animate-pulse" />
                        {language.t("session.agentPanels.state.busy")} ({busyCount()})
                      </button>
                    </Show>
                    <Show when={failedCount() > 0 || filter() === "failed"}>
                      <button
                        type="button"
                        class="inline-flex items-center gap-1 rounded-full px-2.5 py-0.5 text-11-regular tabular-nums transition-colors"
                        classList={{
                          "bg-v2-overlay-simple-overlay-active font-medium text-v2-state-fg-danger":
                            filter() === "failed",
                          "text-v2-state-fg-danger/80 hover:bg-v2-overlay-simple-overlay-hover": filter() !== "failed",
                        }}
                        onClick={() => setFilter(filter() === "failed" ? "all" : "failed")}
                      >
                        {language.t("session.agentPanels.state.failed")} ({failedCount()})
                      </button>
                    </Show>
                    <Show when={completedCount() > 0 || filter() === "completed"}>
                      <button
                        type="button"
                        class="rounded-full px-2.5 py-0.5 text-11-regular tabular-nums transition-colors"
                        classList={{
                          "bg-v2-overlay-simple-overlay-active font-medium text-v2-text-text-base":
                            filter() === "completed",
                          "text-v2-text-text-muted hover:bg-v2-overlay-simple-overlay-hover": filter() !== "completed",
                        }}
                        onClick={() => setFilter(filter() === "completed" ? "all" : "completed")}
                      >
                        {language.t("session.agentPanels.state.completed")} ({completedCount()})
                      </button>
                    </Show>
                    <Show when={hasGrandchildren() && filter() === "all" && !search().trim()}>
                      <button
                        type="button"
                        class="rounded-full border border-v2-border-border-base px-2.5 py-0.5 text-11-regular text-v2-text-text-muted transition-colors hover:bg-v2-overlay-simple-overlay-hover hover:text-v2-text-text-base"
                        onClick={toggleAllBranches}
                      >
                        {language.t(
                          allBranchesExpanded()
                            ? "session.agentPanels.tree.collapseBranches"
                            : "session.agentPanels.tree.expandBranches",
                        )}
                      </button>
                    </Show>
                  </div>
                  <div class="relative flex min-w-[180px] max-w-[260px] flex-1 items-center">
                    <input
                      type="search"
                      value={search()}
                      placeholder={language.t("session.agentPanels.tree.search")}
                      onInput={(e) => setSearch(e.currentTarget.value)}
                      class="h-6 w-full rounded-md border border-v2-border-border-base bg-v2-background-bg-base px-2.5 text-11-regular text-v2-text-text-base placeholder:text-v2-text-text-faint focus:border-v2-border-border-focus focus:outline-none"
                    />
                  </div>
                </div>
              </Show>

              <div
                ref={treeScrollRef}
                role="tree"
                data-scrollable={props.inline ? undefined : ""}
                data-testid="session-agent-tree-nodes"
                class="py-1"
                classList={{
                  "max-h-[min(320px,50vh)] overflow-y-auto": !props.inline,
                }}
              >
                <For each={filteredIds()}>
                  {(nodeId) => {
                    const node = () => byID().get(nodeId)
                    return (
                      <Show when={node()}>
                        {(item) => {
                          const kind = () => relation(item())
                          const current = () => kind() === "current"
                          const childCount = () => childCountByParent().get(item().id) ?? 0
                          const isExpandableBranch = () => item().depth >= 1 && childCount() > 0
                          const branchOpen = () => isBranchExpanded(item().id)

                          return (
                            <div
                              role="treeitem"
                              aria-current={current() ? "true" : undefined}
                              aria-selected={current() ? "true" : undefined}
                              aria-expanded={isExpandableBranch() ? branchOpen() : undefined}
                              data-session-id={item().id}
                              data-relation={kind()}
                              class="flex h-7 w-full items-center gap-1.5 pe-4 text-left text-12-regular transition-colors hover:bg-v2-overlay-simple-overlay-hover"
                              classList={{
                                "font-[530] text-v2-text-text-base bg-v2-overlay-simple-overlay-active/50": current(),
                                "text-v2-text-text-muted": !current(),
                              }}
                              style={{ "padding-inline-start": `${16 + Math.min(item().depth, 6) * 18}px` }}
                            >
                              <Show when={item().depth > 0}>
                                <span
                                  class="shrink-0 select-none font-mono text-[11px] text-v2-text-text-faint/60"
                                  aria-hidden="true"
                                >
                                  └─
                                </span>
                              </Show>
                              <Show when={isExpandableBranch()}>
                                <button
                                  type="button"
                                  aria-label={language.t(
                                    branchOpen()
                                      ? "session.agentPanels.tree.collapseBranches"
                                      : "session.agentPanels.tree.expandBranches",
                                  )}
                                  class="inline-flex h-5 shrink-0 items-center gap-0.5 rounded bg-v2-surface-surface-muted px-1.5 font-mono text-[10px] tabular-nums text-v2-text-text-muted transition-colors hover:bg-v2-overlay-simple-overlay-active hover:text-v2-text-text-base"
                                  onClick={(e) => {
                                    e.stopPropagation()
                                    toggleBranch(item().id)
                                  }}
                                >
                                  <Icon
                                    name={branchOpen() ? "chevron-down" : "outline-chevron-right"}
                                    size="small"
                                    class="shrink-0"
                                  />
                                  <span>{branchOpen() ? childCount() : `+${childCount()}`}</span>
                                </button>
                              </Show>
                              <button
                                type="button"
                                disabled={current()}
                                title={language.t("session.agentPanels.tree.open", { title: title(item()) })}
                                class="flex h-full min-w-0 flex-1 items-center gap-2 text-left disabled:opacity-100"
                                onClick={() => {
                                  if (!current()) {
                                    setCollapsed(true)
                                    open(props.sessionID, item().id)
                                  }
                                }}
                              >
                                <span
                                  class="size-1.5 shrink-0 rounded-full"
                                  classList={{
                                    "bg-v2-state-fg-success animate-pulse": item().state === "busy",
                                    "bg-v2-state-fg-danger":
                                      item().state === "failed" || item().state === "interrupted",
                                    "bg-v2-state-fg-warning": item().state === "suspended",
                                    "bg-v2-text-text-faint/40":
                                      item().state !== "busy" &&
                                      item().state !== "failed" &&
                                      item().state !== "interrupted" &&
                                      item().state !== "suspended",
                                  }}
                                />
                                <span
                                  class="w-16 shrink-0 truncate text-11-regular capitalize"
                                  classList={{
                                    "font-semibold text-v2-text-text-base": current(),
                                    "text-v2-text-text-faint": !current(),
                                  }}
                                >
                                  {language.t(`session.agentPanels.relation.${kind()}`)}
                                </span>
                                <Show when={item().depth > 0}>
                                  <span class="shrink-0 rounded bg-v2-surface-surface-muted px-1 font-mono text-[10px] tabular-nums text-v2-text-text-faint">
                                    {language.t("session.agentPanels.tree.depth", { depth: String(item().depth) })}
                                  </span>
                                </Show>
                                <span class="min-w-0 flex-1 truncate" dir="auto">
                                  {title(item())}
                                </span>
                                <Show when={item().agent}>
                                  <span class="hidden shrink-0 rounded bg-v2-surface-surface-muted px-1.5 py-0.5 text-[11px] capitalize text-v2-text-text-muted sm:inline-block">
                                    {item().agent}
                                  </span>
                                </Show>
                                <Show when={item().model}>
                                  {(model) => (
                                    <span
                                      title={model()}
                                      class="hidden max-w-[160px] shrink-0 truncate font-mono text-[11px] text-v2-text-text-faint md:inline-block"
                                    >
                                      {shortModelName(model())}
                                    </span>
                                  )}
                                </Show>
                                <span
                                  class="w-20 shrink-0 text-end text-11-regular"
                                  classList={{
                                    "font-medium text-v2-state-fg-success": item().state === "busy",
                                    "font-medium text-v2-state-fg-danger":
                                      item().state === "failed" || item().state === "interrupted",
                                    "text-v2-state-fg-warning": item().state === "suspended",
                                    "text-v2-text-text-faint":
                                      item().state !== "busy" &&
                                      item().state !== "failed" &&
                                      item().state !== "interrupted" &&
                                      item().state !== "suspended",
                                  }}
                                >
                                  {language.t(`session.agentPanels.state.${item().state}`)}
                                </span>
                                <Show when={elapsed(item())}>
                                  {(ms) => (
                                    <span class="w-16 shrink-0 text-end text-11-regular tabular-nums whitespace-nowrap text-v2-text-text-faint">
                                      {elapsedLabel(ms())}
                                    </span>
                                  )}
                                </Show>
                              </button>
                            </div>
                          )
                        }}
                      </Show>
                    )
                  }}
                </For>
              </div>
            </div>
          </Show>
        </div>
      </Show>
    </>
  )
}
