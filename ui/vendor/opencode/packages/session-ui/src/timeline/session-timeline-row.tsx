import type {
  SessionMessageAssistant,
  SessionMessageInfo,
  SessionMessageUser,
  SessionStatus,
} from "@opencode/client/promise"
import { useI18n } from "@opencode/ui/context/i18n"
import { Option, Predicate, Schema } from "effect"
import {
  For,
  Match,
  Show,
  Switch,
  createEffect,
  createMemo,
  createSignal,
  onCleanup,
  untrack,
  type Accessor,
  type JSX,
} from "solid-js"
import { DiffChanges } from "@opencode/ui/diff-changes"
import { Icon } from "@opencode/ui/icon"
import { IconButton } from "@opencode/ui/icon-button"
import { Tooltip } from "@opencode/ui/tooltip"
import { Spring, frameLoop, prefersReducedMotion } from "@opencode/ui/motion"
import { springStyle } from "@opencode/ui/motion/solid"
import { Dynamic } from "solid-js/web"
import type { SessionUserActions, SessionUserAttachmentReference, SessionUserComment } from "../actions"
import { useData } from "../context"
import { useSources } from "../context/sources"
import { turnSources } from "../sources/sources"
import { TimelineSeparator } from "../components/timeline-separator"
import {
  SessionAssistantContent,
  SessionContextToolGroup,
  SessionFileToolGroup,
  SessionReadToolGroup,
  SessionShellMessage,
  SessionUserMessage,
  currentContentDefaultOpen,
} from "../message/current-message"
import { relativeTime, useNow } from "../message/relative-time"
import {
  AssistantReasoningContent,
  SessionCompactionMessage,
  SessionCompactionQueued,
  writeClipboard,
} from "../message/message-content"
import { taskAgentName, ToolRegistry, type ContextGroupPart } from "../tools/tool-renderer"
import { GenericTool } from "../components/basic-tool"
import { SessionRetry } from "../components/session-retry"
import { SessionError } from "../components/session-error"
import { isPeerNotice, timelineCategory, type TimelineDetail } from "./detail"
import { currentToolFailed } from "../message/current-tool-state"
import { userMessageRuns, userRunSpring } from "./user-runs"
import { attached } from "../components/message-file"
import {
  createReactiveTimelineProjection,
  Timeline,
  TimelineRow,
  unwrapErrorMessage,
  type PartRef,
  type ReasoningMode,
} from "./projection"

const emptyAssistantMessages: SessionMessageAssistant[] = []

// Past this many files, the rest of a turn's changes wait behind "Show all".
const turnDiffFileLimit = 5

const PEER_RELATIVE_TIME_KEYS = {
  minute: "ui.message.time.minutesAgo",
  hour: "ui.message.time.hoursAgo",
  day: "ui.message.time.daysAgo",
} as const

function readPeerMetadata(value: SessionMessageInfo | undefined) {
  if (value?.type !== "synthetic" || !isPeerNotice(value)) return undefined
  const metadata = value.metadata
  const from = Predicate.isString(metadata?.from) && metadata.from ? metadata.from : "peer"
  const agent = Predicate.isString(metadata?.agent) && metadata.agent ? metadata.agent : undefined
  const kind = Predicate.isString(metadata?.kind) && metadata.kind ? metadata.kind : undefined
  const replyTo = Predicate.isString(metadata?.replyTo) && metadata.replyTo ? metadata.replyTo : undefined

  return {
    id: value.id,
    from,
    fromUser: from === "user",
    agent,
    kind,
    /** A reply to an earlier question: it shows an Answer chip instead of a question label. */
    answer: kind === "answer",
    replyTo,
    /** The inbox item was cancelled; the row stays in history with a chip and no actions. */
    cancelled: metadata?.cancelled === true,
    text: value.text,
    created: value.time.created,
  }
}

const decodeMetadataFile = Schema.decodeUnknownOption(Schema.Struct({ file: Schema.String }))

type Projection = ReturnType<typeof createReactiveTimelineProjection>

type FramedTimelineRow = Exclude<TimelineRow.TimelineRow, TimelineRow.TurnGap>

type Outcome = "completed" | "error" | "cancelled"

export type SessionUserPresentation = {
  displayText?: string
  comments?: SessionUserComment[]
  references?: SessionUserAttachmentReference[]
  /** A voice call's agent sent it on the user's behalf. */
  voice?: boolean
}

export function createSessionTimelineRowRenderer(input: {
  sessionID: Accessor<string>
  status: Accessor<SessionStatus>
  projection: Projection
  presentation: (message: SessionMessageUser) => SessionUserPresentation | undefined
  actions?: SessionUserActions
  reasoningMode: Accessor<ReasoningMode>
  shellToolDefaultOpen: Accessor<boolean>
  editToolDefaultOpen: Accessor<boolean>
  timelineDetail?: Accessor<TimelineDetail>
  foldUserMessages?: Accessor<boolean>
  disclosure: {
    value: (key: string) => boolean | undefined
    set: (key: string, open: boolean) => void
    patchGroupKeys?: Map<string, string>
  }
  centered?: Accessor<boolean>
  padding?: Accessor<string>
  anchor?: (messageID: string) => string | undefined
  /** Scrolls to and expands the tool call named by its ID or by the shell ID a backgrounded shell call reports. */
  revealPart?: (target: string) => boolean
}) {
  const i18n = useI18n()
  const data = useData()
  const sourcesHost = useSources()
  // Cached timelines retain file-change subgroup identities alongside their disclosure choices.
  const patchGroupKeys = input.disclosure.patchGroupKeys ?? new Map<string, string>()
  const patchPartKeys = new WeakMap<SessionMessageAssistant["content"][number], string>()

  const patchOwners = createMemo(() => {
    const owners = new Map<string, string>()
    const rows = input.projection.rows()
    // Track status changes before a group is first opened: a failed file change can
    // split an existing group without changing the projection's row identities.
    rows.forEach((row) => {
      if (!Predicate.isTagged(row, "AssistantPart") || row.group.type !== "context") return
      row.group.refs.forEach((ref) => {
        const content = Timeline.resolveContent(input.projection.messageByID().get(ref.messageID), ref.partID)

        if (
          content?.type !== "tool" ||
          !["edit", "write", "patch"].includes(content.name) ||
          content.state.status === "error"
        )
          return
        const part = `${ref.messageID}:${ref.partID}`
        const key = patchGroupKeys.get(part)

        if (key && !owners.has(key)) owners.set(key, part)
      })
    })

    return owners
  })

  const runs = createMemo(() => {
    const messages = input.projection.messageByID()
    const context = input.projection.userContextByID()

    return userMessageRuns(input.projection.rows(), [...messages.values()], (id) => {
      const message = messages.get(id)
      const user = context.get(id)

      if (message?.type !== "user" || !user) return undefined
      const status = input.actions?.pending?.status(id)
      const presentation = input.presentation(message)

      return {
        kind: status === "steering" || status === "stranded" ? "pending" : "sent",
        agent: user.agent,
        model: user.model,
        text: !!(presentation?.displayText ?? message.text),
        attachments: (message.files ?? []).some(attached) || !!presentation?.references?.length,
      }
    })
  })

  // A steer sent while the timeline is open grows in. Messages already there when a session loads, and rows the
  // virtualizer remounts, appear as they are.
  const seen = { sessionID: "", ids: new Set<string>() }

  const arrivals = createMemo(() => {
    const sessionID = input.sessionID()
    const loaded = seen.sessionID === sessionID && seen.ids.size > 0
    const arrived = new Set<string>()

    if (seen.sessionID !== sessionID) seen.ids.clear()
    seen.sessionID = sessionID
    input.projection.rows().forEach((row) => {
      if ((!Predicate.isTagged(row, "UserMessage") && !Predicate.isTagged(row, "PeerMessage")) || seen.ids.has(row.userMessageID))
        return
      seen.ids.add(row.userMessageID)

      if (loaded) arrived.add(row.userMessageID)
    })

    return arrived
  })

  // The input's pending status lands after its row, so a message sent while the session works counts as a steer.
  const arriving = (messageID: string) => untrack(() => arrivals().delete(messageID) && input.status().type !== "idle")

  const run = (messageID: string) =>
    createMemo(() => runs().get(messageID), undefined, {
      equals: (a, b) => a?.above === b?.above && a?.below === b?.below,
    })

  const workingTurn = (messageID: string) =>
    input.status().type !== "idle" && input.projection.activeMessageID() === messageID

  const duration = (messageID: string) => {
    const opener = input.projection.messageByID().get(messageID)

    if (opener?.type !== "user" && (opener?.type !== "synthetic" || !isPeerNotice(opener))) return null

    const completed = (input.projection.assistantMessagesByParent().get(messageID) ?? emptyAssistantMessages).reduce<
      number | undefined
    >((latest, message) => {
      if (message.time.completed === undefined) return latest

      return latest === undefined ? message.time.completed : Math.max(latest, message.time.completed)
    }, undefined)

    if (completed === undefined || completed < opener.time.created) return undefined

    return completed - opener.time.created
  }

  const copyContentID = (messageID: string) => {
    if (workingTurn(messageID)) return null

    const message = input.projection
      .assistantMessagesByParent()
      .get(messageID)
      ?.findLast((message) => message.content.some((content) => content.type === "text" && !!content.text.trim()))

    return message
      ? Timeline.contentEntries(message).findLast(
          (entry) => entry.content.type === "text" && !!entry.content.text.trim(),
        )?.id
      : undefined
  }

  const padding = () => input.padding?.() ?? "px-4 md:px-6"

  const indexGroupContents = (refs: PartRef[]) => {
    const result = new Map<string, Map<string, SessionMessageAssistant["content"][number]>>()
    refs.forEach((ref) => {
      if (result.has(ref.messageID)) return
      const contents = new Map<string, SessionMessageAssistant["content"][number]>()
      const message = input.projection.messageByID().get(ref.messageID)

      if (message?.type === "assistant") {
        Timeline.contentEntries(message).forEach((entry) => {
          // Match resolveContent's first entry when content IDs repeat.
          if (!contents.has(entry.id)) contents.set(entry.id, entry.content)
        })
      }

      result.set(ref.messageID, contents)
    })

    return result
  }

  const renderAssistant = (row: Accessor<TimelineRow.AssistantPart>, onSizeChange?: () => void) => {
    if (row().group.type === "context") {
      const parts = createMemo(() => {
        const group = row().group

        if (group.type !== "context") return []
        const contents = indexGroupContents(group.refs)
        const lastAssistant = input.projection.assistantMessagesByParent().get(row().userMessageID)?.at(-1)

        return group.refs.flatMap<ContextGroupPart>((ref) => {
          const content = contents.get(ref.messageID)?.get(ref.partID)

          if (content?.type === "tool") {
            patchPartKeys.set(content, `${ref.messageID}:${ref.partID}`)

            return [content]
          }

          if (content?.type === "reasoning")
            return [
              {
                ...content,
                id: ref.partID,
                streaming:
                  workingTurn(row().userMessageID) &&
                  input.status().type === "busy" &&
                  lastAssistant?.id === ref.messageID &&
                  lastAssistant.time.completed === undefined &&
                  !lastAssistant.error &&
                  !lastAssistant.retry &&
                  lastAssistant.content.at(-1) === content &&
                  content.time?.completed === undefined,
              },
            ]
          const message = input.projection.messageByID().get(ref.messageID)

          if (content?.type === "text" && message?.type === "assistant")
            return [
              {
                type: "text",
                id: ref.partID,
                render: () => <SessionAssistantContent message={message} content={content} contentID={ref.partID} />,
              },
            ]

          if (ref.messageID !== ref.partID || !message) return []

          if (message.type === "shell")
            return [{ type: "shell", id: ref.partID, render: () => <Shell messageID={ref.messageID} grouped /> }]

          if (message.type !== "assistant" && message.type !== "user")
            return [{ type: "notice", id: ref.partID, render: () => <Notice messageID={ref.messageID} grouped /> }]

          return []
        })
      })

      const key = () => `context:${row().group.key}`

      return (
        <div class="contents" data-notices-only={parts().every((part) => part.type === "notice") ? "" : undefined}>
          <SessionContextToolGroup
            parts={parts()}
            patchGroupKey={(tools) => {
              const parts = tools.map((tool) => patchPartKeys.get(tool)!)

              // After a split, only the subgroup with the earliest surviving member keeps the old anchor.
              const key =
                parts
                  .map((part) => patchGroupKeys.get(part))
                  .find((key) => key !== undefined && parts.includes(patchOwners().get(key)!)) ?? parts[0]!

              parts.forEach((part) => patchGroupKeys.set(part, key))

              return key
            }}
            reasoningDefaultOpen={
              input.timelineDetail
                ? input.timelineDetail().thinking.details === "expanded"
                : input.reasoningMode() === "full"
            }
            reasoningOpen={(id) => input.disclosure.value(id)}
            onReasoningOpenChange={(id, open) => input.disclosure.set(id, open)}
            toolDefaultOpen={(tool) =>
              input.timelineDetail || tool.name === "execute" ? contentDefaultOpen(tool) : false
            }
            toolOpen={(id) => input.disclosure.value(`${row().group.key}:tool:${id}`)}
            onToolOpenChange={(id, open) => input.disclosure.set(`${row().group.key}:tool:${id}`, open)}
            fileOpen={(path) =>
              input.disclosure.value(`patch:${path}`) ?? input.timelineDetail?.().edit.details === "expanded"
            }
            onFileOpenChange={(path, open) => input.disclosure.set(`patch:${path}`, open)}
            open={input.disclosure.value(key()) === true}
            busy={
              workingTurn(row().userMessageID) &&
              input.projection.lastAssistantGroupKey().get(row().userMessageID) === row().group.key
            }
            onOpenChange={(open) => input.disclosure.set(key(), open)}
            onSizeChange={onSizeChange}
          />
        </div>
      )
    }

    if (row().group.type === "read") {
      const tools = createMemo(() => {
        const group = row().group

        if (group.type !== "read") return []
        const contents = indexGroupContents(group.refs)

        return group.refs.flatMap((ref) => {
          const content = contents.get(ref.messageID)?.get(ref.partID)

          return content?.type === "tool" ? [content] : []
        })
      })

      return (
        <SessionReadToolGroup
          tools={tools()}
          open={input.disclosure.value(row().group.key)}
          onOpenChange={(open) => input.disclosure.set(row().group.key, open)}
          onSizeChange={onSizeChange}
        />
      )
    }

    if (row().group.type === "file") {
      const tools = createMemo(() => {
        const group = row().group

        if (group.type !== "file") return []
        const contents = indexGroupContents(group.refs)

        return group.refs.flatMap((ref) => {
          const content = contents.get(ref.messageID)?.get(ref.partID)

          return content?.type === "tool" ? [content] : []
        })
      })

      const firstPath = createMemo(() => {
        const tool = tools()[0]

        if (!tool || !("metadata" in tool.state)) return undefined
        const files = tool.state.metadata?.files

        if (!Array.isArray(files)) return undefined

        return Option.getOrUndefined(decodeMetadataFile(files[0]))?.file
      })

      return (
        <SessionFileToolGroup
          tools={tools()}
          fileOpen={(path) => {
            const open = input.disclosure.value(`${row().group.key}:file:${path}`)

            if (open !== undefined) return open

            if (input.timelineDetail) return input.timelineDetail().edit.details === "expanded"

            if (tools()[0]?.name !== "edit" || path !== firstPath()) return false

            return input.disclosure.value(row().group.key) ?? input.editToolDefaultOpen()
          }}
          onFileOpenChange={(path, open) => input.disclosure.set(`${row().group.key}:file:${path}`, open)}
          onSizeChange={onSizeChange}
        />
      )
    }

    const ref = createMemo(() => {
      const group = row().group

      return group.type === "part" ? group.ref : undefined
    })

    const message = createMemo(() => {
      const current = ref()
      const message = current ? input.projection.messageByID().get(current.messageID) : undefined

      return message?.type === "assistant" ? message : undefined
    })

    const content = createMemo(() => {
      const current = ref()

      return current ? Timeline.resolveContent(message(), current.partID) : undefined
    })

    const defaultOpen = createMemo(() => {
      const item = content()

      return item ? contentDefaultOpen(item) : undefined
    })

    // Only the finished turn's footer part derives them, and only while the host shows sources.
    const turnSourcesValue = createMemo(() => {
      const current = ref()

      if (!sourcesHost?.enabled || !current || copyContentID(row().userMessageID) !== current.partID) return undefined

      const value = turnSources(
        input.projection.assistantMessagesByParent().get(row().userMessageID) ?? emptyAssistantMessages,
      )

      return value.steps.length > 0 ? value : undefined
    })

    const disclosureKey = () => (content()?.type === "reasoning" ? ref()!.partID : row().group.key)

    return (
      <Show when={message()}>
        {(message) => (
          <Show when={content()}>
            {(content) => (
              <SessionAssistantContent
                message={message()}
                content={content()}
                contentID={ref()!.partID}
                showAssistantCopyPartID={copyContentID(row().userMessageID)}
                turnDurationMs={duration(row().userMessageID)}
                sources={turnSourcesValue()}
                onOpenSources={() => sourcesHost?.open(row().userMessageID)}
                defaultOpen={defaultOpen()}
                toolOpen={input.disclosure.value(disclosureKey()) ?? defaultOpen()}
                onToolOpenChange={(open) => input.disclosure.set(disclosureKey(), open)}
                onContentRendered={onSizeChange}
              />
            )}
          </Show>
        )}
      </Show>
    )
  }

  function contentDefaultOpen(item: SessionMessageAssistant["content"][number]) {
    if (item.type === "tool" && item.name === "execute") return true

    if (input.timelineDetail) {
      const category = timelineCategory(item)

      if (
        category &&
        input.timelineDetail()[category].placement === "hidden" &&
        item.type === "tool" &&
        currentToolFailed(item)
      )
        return true

      if (category === "shell" || category === "edit" || category === "thinking")
        return input.timelineDetail()[category].details === "expanded"
    }

    if (item.type === "reasoning") return input.reasoningMode() === "full"

    return currentContentDefaultOpen(item, input.shellToolDefaultOpen(), input.editToolDefaultOpen())
  }

  const notice = (message: SessionMessageInfo) => {
    if (message.type === "agent-switched")
      return {
        label: i18n.t("ui.sessionTimeline.notice.agentChanged"),
        data: message.previous
          ? `${capitalizeAgent(message.previous)} → ${capitalizeAgent(message.agent)}`
          : capitalizeAgent(message.agent),
      }

    if (message.type === "model-switched") return undefined

    if (message.type === "skill") return { label: i18n.t("ui.tool.skill"), data: message.name }

    if (message.type === "system") {
      const sources = Array.isArray(message.metadata?.instructionSources)
        ? message.metadata.instructionSources.filter(Predicate.isString)
        : undefined

      if (message.metadata?.notice === "instructions" && sources?.length)
        return { label: i18n.t("ui.sessionTimeline.notice.instructionsUpdated"), items: sources }
      const prefix = "Instructions updated: "

      if (message.description?.startsWith(prefix)) {
        const keys = message.description
          .slice(prefix.length)
          .split(",")
          .map((s) => s.trim())
          .filter(Boolean)

        return {
          label: i18n.t("ui.sessionTimeline.notice.instructionsUpdated"),
          items: keys,
        }
      }

      return { label: message.description ?? message.text }
    }

    if (message.type !== "synthetic") return undefined

    if (message.metadata?.notice === "restart") return { label: i18n.t("ui.sessionTimeline.notice.restart") }

    if (message.description === "Continuing after restart")
      return { label: i18n.t("ui.sessionTimeline.notice.restart") }
    const source = message.metadata?.source

    if (source === "subagent" || source === "shell") {
      const agent = message.metadata?.agent

      const actor =
        source === "shell"
          ? i18n.t("ui.tool.shell")
          : ((Predicate.isString(agent) ? taskAgentName(agent, data.store.agent) : undefined) ??
            i18n.t("ui.tool.agent.default"))

      const state = message.metadata?.state
      const outcome: Outcome = state === "error" || state === "cancelled" ? state : "completed"

      return {
        label: i18n.t(outcomeNoticeKey(outcome), { actor }),
        data: message.description,
        outcome,
        icon: outcomeNoticeIcon(outcome),
      }
    }

    return { label: message.description ?? message.text }
  }

  const Frame = (props: { row: FramedTimelineRow; grow?: boolean; children: JSX.Element }) => (
    <div
      ref={(el) => {
        if (props.grow) growIn(el)
      }}
      id={
        Predicate.isTagged(props.row, "UserMessage") || Predicate.isTagged(props.row, "PeerMessage")
          ? input.anchor?.(props.row.userMessageID)
          : undefined
      }
      data-message-id={props.row.userMessageID}
      data-timeline-row={props.row._tag}
      data-timeline-spacing={Predicate.isTagged(props.row, "AssistantPart") ? props.row.spacing : undefined}
      classList={{
        "min-w-0 w-full max-w-full": true,
        "md:max-w-session md:mx-auto": input.centered?.(),
        "pt-2": Predicate.isTagged(props.row, "AssistantPart") && props.row.spacing === "tool",
        "pt-4": Predicate.isTagged(props.row, "AssistantPart") && props.row.spacing === "content",
        "pt-6": Predicate.isTagged(props.row, "Error"),
      }}
    >
      <div data-component="session-turn" class="min-w-0 w-full relative" style={{ height: "auto" }}>
        {props.children}
      </div>
    </div>
  )

  function PeerMessage(props: { messageID: string; grouped?: boolean }) {
    const message = createMemo(() => input.projection.messageByID().get(props.messageID))
    const peer = createMemo(() => readPeerMetadata(message()))
    const pendingStatus = createMemo(() => input.actions?.pending?.status(props.messageID))
    const queued = createMemo(() => !peer()?.cancelled && pendingStatus() === "queued")
    const waiting = createMemo(
      () => pendingStatus() === "steering" || pendingStatus() === "starting" || pendingStatus() === "stranded",
    )
    const now = useNow()
    const numfmt = createMemo(() => new Intl.NumberFormat(i18n.locale()))
    const datefmt = createMemo(() => new Intl.DateTimeFormat(i18n.locale(), { dateStyle: "medium" }))
    const fulltimefmt = createMemo(() =>
      new Intl.DateTimeFormat(i18n.locale(), { dateStyle: "full", timeStyle: "long" }),
    )
    const [copied, setCopied] = createSignal(false)
    const [updating, setUpdating] = createSignal(false)

    const stamp = createMemo(() => {
      const created = peer()?.created
      if (created === undefined) return ""
      const relative = relativeTime(created, now())

      if (relative.unit === "now") return i18n.t("ui.message.time.justNow")
      if (relative.unit === "date") return datefmt().format(created)

      return i18n.t(PEER_RELATIVE_TIME_KEYS[relative.unit], { count: numfmt().format(relative.count) })
    })

    const copy = async (text: string) => {
      if (!text || !(await writeClipboard(text))) return
      setCopied(true)
      setTimeout(() => setCopied(false), 2000)
    }

    const runPending = async (task: () => Promise<void>) => {
      if (updating()) return
      setUpdating(true)
      try {
        await task()
      } finally {
        setUpdating(false)
      }
    }

    return (
      <Show when={peer()}>
        {(entry) => (
          <div
            data-slot="session-turn-message-container"
            data-component="peer-message"
            data-type="peer"
            data-message-id={entry().id}
            data-pending={waiting() ? "true" : undefined}
            data-queued={queued() ? "true" : undefined}
            data-peer-kind={entry().answer ? "answer" : "question"}
            data-reply-to={entry().replyTo}
            data-cancelled={entry().cancelled ? "true" : undefined}
            class={`w-full ${props.grouped ? "py-1" : padding()} flex flex-col items-end`}
          >
            <div class="w-fit max-w-[min(86%,64ch)] ml-auto flex flex-col items-end gap-1">
              <div
                data-slot="peer-message-bubble"
                data-status={pendingStatus() ?? "sent"}
                classList={{
                  "w-full rounded-[10px] px-3 py-2 text-14-regular transition-colors": true,
                  "border border-dashed border-v2-border-border-strong/80 bg-v2-background-bg-layer-01/85 text-v2-text-text-muted":
                    queued(),
                  "border border-v2-border-border-base/70 bg-v2-background-bg-layer-01 text-v2-text-text-muted":
                    waiting(),
                  "border border-v2-border-border-base/80 bg-v2-background-bg-layer-02 text-v2-text-text-base":
                    !queued() && !waiting(),
                }}
              >
                <div class="mb-1 flex flex-wrap items-center gap-1.5 text-11-regular text-v2-text-text-muted">
                  <Icon
                    name={waiting() ? "outline-steer" : "chat-bubble"}
                    size="small"
                    class="shrink-0 text-v2-icon-icon-faint"
                  />
                  <span class="rounded-full bg-v2-background-bg-layer-03/70 px-1.5 py-px font-[530] text-v2-text-text-accent">
                    {entry().fromUser
                      ? i18n.t("ui.sessionTimeline.notice.peerFromYou")
                      : i18n.t("ui.sessionTimeline.notice.peerFrom", { sender: entry().from })}
                  </span>
                  <Show when={entry().agent}>
                    {(agent) => (
                      <span class="rounded bg-v2-background-bg-layer-03/50 px-1 py-px capitalize text-v2-text-text-faint">
                        {agent()}
                      </span>
                    )}
                  </Show>
                  <Show when={entry().answer}>
                    <span
                      data-slot="peer-message-answer"
                      class="rounded bg-v2-background-bg-layer-03/50 px-1 py-px text-v2-text-text-faint"
                    >
                      {i18n.t("ui.sessionTimeline.peer.answer")}
                    </span>
                  </Show>
                  <Show when={!entry().answer && entry().kind}>
                    {(kind) => <span class="text-v2-text-text-faint">{kind()}</span>}
                  </Show>
                  <Show when={entry().cancelled}>
                    <span
                      data-slot="peer-message-cancelled"
                      class="ml-auto rounded-full border border-v2-border-border-base px-1.5 py-px font-[530] text-v2-text-text-faint"
                    >
                      {i18n.t("ui.sessionTimeline.peer.cancelled")}
                    </span>
                  </Show>
                  <Show when={queued()}>
                    <span
                      data-slot="peer-message-status"
                      data-status="queued"
                      class="ml-auto rounded-full border border-v2-border-border-base px-1.5 py-px font-[530] text-v2-text-text-muted"
                    >
                      {i18n.t("ui.message.queued")}
                    </span>
                  </Show>
                  <Show when={waiting()}>
                    <span
                      data-slot="peer-message-status"
                      data-status={pendingStatus()}
                      class="ml-auto rounded-full bg-v2-background-bg-layer-03/70 px-1.5 py-px font-[530] text-v2-text-text-muted"
                    >
                      {i18n.t("ui.message.pending")}
                    </span>
                  </Show>
                </div>
                <div
                  data-slot="peer-message-text"
                  class="select-text whitespace-pre-wrap break-words leading-5"
                  dir="auto"
                >
                  {entry().text}
                </div>
                <Show when={queued() && !entry().cancelled && input.actions?.pending}>
                  {(pendingActions) => (
                    <div class="mt-2 flex flex-wrap items-center justify-between gap-2 border-t border-v2-border-border-base/60 pt-1.5 text-11-regular text-v2-text-text-muted">
                      <span>{i18n.t("ui.sessionTimeline.peer.queuedHint")}</span>
                      <div class="flex items-center gap-1.5">
                        <Show when={pendingActions().steer}>
                          {(steer) => (
                            <button
                              type="button"
                              disabled={updating()}
                              onMouseDown={(event) => event.preventDefault()}
                              onClick={() =>
                                void runPending(() => steer()({ sessionID: input.sessionID(), messageID: entry().id }))
                              }
                              class="inline-flex h-6 items-center gap-1 rounded-md border border-v2-border-border-base bg-v2-background-bg-layer-02 px-2 text-11-regular font-[530] text-v2-text-text-base hover:bg-v2-background-bg-layer-03 disabled:opacity-50"
                            >
                              {i18n.t("ui.sessionTimeline.peer.steerNow")}
                            </button>
                          )}
                        </Show>
                        <button
                          type="button"
                          disabled={updating()}
                          onMouseDown={(event) => event.preventDefault()}
                          onClick={() =>
                            void runPending(() =>
                              pendingActions().remove({ sessionID: input.sessionID(), messageID: entry().id }),
                            )
                          }
                          class="inline-flex h-6 items-center gap-1 rounded-md px-2 text-11-regular text-v2-text-text-muted hover:bg-v2-background-bg-layer-02 hover:text-v2-text-text-base disabled:opacity-50"
                        >
                          {i18n.t("ui.sessionTimeline.peer.cancel")}
                        </button>
                      </div>
                    </div>
                  )}
                </Show>
              </div>
              <div class="flex items-center justify-end gap-1.5 px-0.5 text-11-regular text-v2-text-text-faint">
                <Show when={waiting() && input.actions?.pending}>
                  {(pendingActions) => (
                    <>
                      <Tooltip appearance="compact" value={i18n.t("ui.message.moveToQueue")} placement="top" gutter={4}>
                        <IconButton
                          icon={<Icon name="arrow-down-to-line" size="small" />}
                          size="small"
                          variant="ghost-muted"
                          disabled={updating()}
                          onMouseDown={(event) => event.preventDefault()}
                          onClick={() =>
                            void runPending(() =>
                              pendingActions().queue({ sessionID: input.sessionID(), messageID: entry().id }),
                            )
                          }
                          aria-label={i18n.t("ui.message.moveToQueue")}
                        />
                      </Tooltip>
                      <Tooltip appearance="compact" value={i18n.t("ui.message.deletePending")} placement="top" gutter={4}>
                        <IconButton
                          icon={<Icon name="outline-trash" size="small" />}
                          size="small"
                          variant="ghost-muted"
                          disabled={updating()}
                          onMouseDown={(event) => event.preventDefault()}
                          onClick={() =>
                            void runPending(() =>
                              pendingActions().remove({ sessionID: input.sessionID(), messageID: entry().id }),
                            )
                          }
                          aria-label={i18n.t("ui.message.deletePending")}
                        />
                      </Tooltip>
                      <span>{i18n.t("ui.sessionTimeline.peer.steeringHint")}</span>
                    </>
                  )}
                </Show>
                <Show when={!waiting() && !queued()}>
                  <span title={fulltimefmt().format(entry().created)}>{stamp()}</span>
                </Show>
                <Tooltip
                  appearance="compact"
                  value={copied() ? i18n.t("ui.message.copied") : i18n.t("ui.message.copyMessage")}
                  placement="top"
                  gutter={4}
                >
                  <IconButton
                    icon={<Icon name={copied() ? "check" : "copy"} size="small" />}
                    size="small"
                    variant="ghost-muted"
                    onMouseDown={(event) => event.preventDefault()}
                    onClick={() => void copy(entry().text)}
                    aria-label={copied() ? i18n.t("ui.message.copied") : i18n.t("ui.message.copyMessage")}
                  />
                </Tooltip>
              </div>
            </div>
          </div>
        )}
      </Show>
    )
  }

  function Notice(props: { messageID: string; grouped?: boolean }) {
    const inset = () => (props.grouped ? "" : padding())
    const message = createMemo(() => input.projection.messageByID().get(props.messageID))

    const compaction = createMemo(() => {
      const value = message()

      return value?.type === "compaction" ? value : undefined
    })

    const compactionError = createMemo(() => {
      const value = compaction()

      if (value?.status !== "failed") return ""

      return unwrapErrorMessage(value.error.message)
    })

    const moved = createMemo(() => {
      const value = message()

      return value?.type === "location-switched" ? value : undefined
    })

    const model = createMemo(() => {
      const value = message()

      if (value?.type !== "model-switched") return undefined
      const cleanVariant = (variant: string | undefined) => {
        const trimmed = variant?.trim()
        return trimmed && trimmed.toLowerCase() !== "default" ? trimmed : undefined
      }
      const nextVariant = cleanVariant(value.model.variant)
      const prevVariant = cleanVariant(value.previous?.variant)
      const match = data.store.provider?.all?.get(value.model.providerID)
      const variantOnly = value.previous?.providerID === value.model.providerID && value.previous.id === value.model.id
      if (variantOnly && prevVariant === nextVariant) return undefined
      const variant = nextVariant ?? "default"

      return {
        providerID: value.model.providerID,
        variant: variantOnly ? undefined : nextVariant,
        label: variantOnly
          ? variant === "default"
            ? i18n.t("ui.sessionTimeline.notice.variantDefault")
            : i18n.t("ui.sessionTimeline.notice.variantSwitched", { variant })
          : i18n.t("ui.sessionTimeline.notice.modelSwitched", {
              model: match?.models?.[value.model.id]?.name ?? value.model.id,
            }),
      }
    })

    const peer = createMemo(() => readPeerMetadata(message()))

    const content = createMemo(() => {
      const value = message()

      return value ? notice(value) : undefined
    })

    const childID = createMemo(() => {
      const value = message()

      if (value?.type !== "synthetic" || value.metadata?.source !== "subagent") return
      const id = value.metadata.childID

      if (Predicate.isString(id) && id) return id
    })

    const href = createMemo(() => {
      const id = childID()

      if (id) return data.sessionHref?.(id)
    })

    // A finished background shell jumps to its call, as its activity pill does.
    const shellID = createMemo(() => {
      const value = message()

      if (!input.revealPart || value?.type !== "synthetic" || value.metadata?.source !== "shell") return
      const id = value.metadata.shellID

      if (Predicate.isString(id) && id) return id
    })

    const clickable = createMemo(() => !!shellID() || !!(childID() && (data.navigateToSession || href())))

    const open = () => {
      const shell = shellID()

      if (shell) return void input.revealPart?.(shell)
      const id = childID()

      if (id) data.navigateToSession?.(id)
    }

    const navigate = (event: MouseEvent) => {
      if (!shellID() && (!childID() || !data.navigateToSession)) return

      if (event.button !== 0 || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return
      event.preventDefault()
      open()
    }

    const navigateKey = (event: KeyboardEvent) => {
      if (!clickable() || href()) return

      if (event.key !== "Enter" && event.key !== " ") return
      event.preventDefault()
      open()
    }

    return (
      <>
        <Show when={peer()}>
          <PeerMessage messageID={props.messageID} grouped={props.grouped} />
        </Show>
        <Show when={!peer() && compaction()}>
          {(message) => (
            <div data-slot="session-turn-message-container" class={`w-full ${inset()}`}>
              <div data-slot="session-turn-compaction">
                <SessionCompactionMessage message={message()} error={compactionError()} />
              </div>
            </div>
          )}
        </Show>
        <Show
          when={!peer() && moved()}
          fallback={
            <Show
              when={!peer() && model()}
              fallback={
                <Show when={!peer() && content()}>
                  {(content) => (
                    <Show
                      when={content().items?.length}
                      fallback={
                        <Dynamic
                          component={href() ? "a" : "div"}
                          data-slot="session-timeline-notice"
                          data-outcome={content().outcome}
                          class={`block w-full truncate ${props.grouped ? "py-1" : "pt-3 pb-1"} text-13-regular leading-text-compact text-v2-text-text-faint ${inset()}`}
                          classList={{ "cursor-pointer hover:text-v2-text-text-base": clickable() }}
                          href={href()}
                          role={clickable() && !href() ? (shellID() ? "button" : "link") : undefined}
                          tabIndex={clickable() && !href() ? 0 : undefined}
                          onClick={navigate}
                          onKeyDown={navigateKey}
                        >
                          <Show when={content().icon}>
                            {(icon) => (
                              <Icon
                                name={icon()}
                                size="normal"
                                class={`inline-block align-top me-1.5 ${outcomeNoticeColor(content().outcome)}`}
                              />
                            )}
                          </Show>
                          <bdi
                            dir="auto"
                            class={`font-[530] ${outcomeNoticeColor(content().outcome)}`}
                            classList={{ "text-v2-text-text-faint": message()?.type === "agent-switched" }}
                          >
                            {content().label}
                          </bdi>
                          <Show when={content().data}>
                            {(data) => (
                              <span classList={{ "ms-2": message()?.type === "agent-switched" }}>
                                <Show when={message()?.type !== "agent-switched"}>{" · "}</Show>
                                <bdi dir="auto">{data()}</bdi>
                              </span>
                            )}
                          </Show>
                        </Dynamic>
                      }
                    >
                      <div data-slot="session-timeline-notice" class={`w-full py-1 ${inset()}`}>
                        <div class="flex min-h-5 min-w-0 items-center gap-2 overflow-hidden">
                          <bdi
                            dir="auto"
                            class="shrink-0 text-[13px] font-[530] leading-text-compact tracking-[-0.04px] text-v2-text-text-faint"
                          >
                            {content().label}
                          </bdi>
                          <For each={content().items}>
                            {(item) => (
                              <bdi
                                dir="auto"
                                class="min-w-0 truncate text-[13px] font-[440] leading-text-compact tracking-[-0.04px] text-v2-text-text-faint"
                              >
                                {item}
                              </bdi>
                            )}
                          </For>
                        </div>
                      </div>
                    </Show>
                  )}
                </Show>
              }
            >
              {(model) => (
                <div data-slot="session-timeline-notice" data-type="model-switched" class={`w-full py-2 ${inset()}`}>
                  <TimelineSeparator label={model().label} providerID={model().providerID} variant={model().variant} />
                </div>
              )}
            </Show>
          }
        >
          {(message) => (
            <div data-slot="session-timeline-notice" data-type="location-switched" class={`w-full py-2 ${inset()}`}>
              <TimelineSeparator
                label={i18n.t("ui.sessionTimeline.notice.movedTo")}
                value={message().location.directory}
                tooltip={i18n.t("ui.sessionTimeline.notice.movedTooltip")}
              />
            </div>
          )}
        </Show>
      </>
    )
  }

  function Shell(props: { messageID: string; grouped?: boolean }) {
    const message = createMemo(() => {
      const value = input.projection.messageByID().get(props.messageID)

      return value?.type === "shell" ? value : undefined
    })

    const defaultOpen = createMemo(() => {
      if (!input.timelineDetail) return input.shellToolDefaultOpen()
      const value = message()

      return (
        input.timelineDetail().shell.details === "expanded" ||
        (input.timelineDetail().shell.placement === "hidden" &&
          (value?.status === "timeout" || (value?.status === "exited" && value.exit !== undefined && value.exit !== 0)))
      )
    })

    return (
      <Show when={message()}>
        {(message) => (
          <div data-slot="session-turn-message-container" class={`w-full ${props.grouped ? "" : padding()}`}>
            <SessionShellMessage
              message={message()}
              defaultOpen={defaultOpen()}
              open={input.disclosure.value(message().id) ?? (input.timelineDetail ? defaultOpen() : undefined)}
              onOpenChange={(open) => input.disclosure.set(message().id, open)}
            />
          </div>
        )}
      </Show>
    )
  }

  function TurnDiff(props: { row: TimelineRow.TurnDiff; onSizeChange?: () => void }) {
    const render = ToolRegistry.render("patch") ?? GenericTool
    // Until the diff loads, list the paths the turn's steps recorded, without change counts.
    const turn = () => ({ sessionID: input.sessionID(), from: props.row.from })
    const diffs = () => data.turnDiff?.get(turn())
    const paths = () => diffs()?.map((diff) => diff.file) ?? props.row.files
    const showAllKey = () => `turn-diff:${props.row.messageID}`
    const showAll = () => input.disclosure.value(showAllKey()) === true
    const limit = <T,>(items: readonly T[]) => (showAll() ? items : items.slice(0, turnDiffFileLimit))
    const overflow = () => paths().length > turnDiffFileLimit
    const fileKey = (path: string) => `turn-diff:${props.row.messageID}:file:${path}`

    createEffect(() => data.turnDiff?.load(turn()))

    return (
      <Show when={data.turnDiff && paths().length > 0}>
        <div data-slot="session-turn-diffs" class="flex flex-col gap-2 pt-4">
          <div class="flex h-6 min-w-0 items-center gap-2">
            <span class="text-14-medium text-v2-text-text-base tabular-nums">
              {i18n.plural("ui.sessionTurn.diffs.changed", paths().length)}
            </span>
            <DiffChanges changes={[...(diffs() ?? [])]} />
            <Show when={overflow()}>
              <button
                type="button"
                class="ml-auto text-14-regular text-v2-text-text-accent"
                onClick={() => input.disclosure.set(showAllKey(), !showAll())}
              >
                {showAll() ? i18n.t("ui.sessionTurn.diffs.showLess") : i18n.t("ui.sessionTurn.diffs.showAll")}
              </button>
            </Show>
          </div>
          <Dynamic
            component={render}
            tool="patch"
            input={{}}
            metadata={{ files: limit(diffs() ?? []) }}
            pendingFiles={diffs() ? undefined : limit(props.row.files)}
            status="completed"
            fileOpen={(path: string) => input.disclosure.value(fileKey(path))}
            onFileOpenChange={(path: string, open: boolean) => input.disclosure.set(fileKey(path), open)}
            deferContent
            virtualizeDiff={false}
            onContentRendered={props.onSizeChange}
          />
        </div>
      </Show>
    )
  }

  // A run's messages sit as close as the iOS app's (4 px); the gap springs open as a message leaves.
  function TurnGap(props: { messageID: string }) {
    let element: HTMLDivElement | undefined
    const joined = run(props.messageID)
    springStyle(
      () => element,
      () => ({ height: joined()?.above ? 4 : 16 }),
      (el, value) => {
        el.style.height = `${value.height}px`
      },
      // Closing up as a message joins is instant; the joining message grows in instead.
      { ...userRunSpring, snap: () => !!joined()?.above },
    )

    return (
      <div
        ref={element}
        data-timeline-row="TurnGap"
        aria-hidden="true"
        style={{ height: untrack(joined)?.above ? "4px" : "16px" }}
      />
    )
  }

  const render = (row: Accessor<TimelineRow.TimelineRow>, onSizeChange?: () => void) => {
    if (Predicate.isTagged(row(), "TurnGap")) return <TurnGap messageID={row().userMessageID} />

    // The host owns live status and renders this slot itself.
    if (Predicate.isTagged(row(), "Activity")) return null

    if (Predicate.isTagged(row(), "UserMessage")) {
      const current = () => {
        const value = row()

        if (!Predicate.isTagged(value, "UserMessage")) throw new Error("Expected a user-message timeline row")

        return value
      }

      const message = createMemo(() => {
        const value = input.projection.messageByID().get(current().userMessageID)

        return value?.type === "user" ? value : undefined
      })

      const context = createMemo(() => input.projection.userContextByID().get(current().userMessageID))
      const joined = run(current().userMessageID)
      const grow = arriving(current().userMessageID)

      return (
        <Frame row={current()} grow={grow}>
          <Show when={message()}>
            {(message) => {
              const presentation = () => input.presentation(message())

              return (
                <div data-slot="session-turn-message-container" class={`w-full ${padding()}`}>
                  <div data-slot="session-turn-message-content" aria-live="off">
                    <SessionUserMessage
                      sessionID={input.sessionID()}
                      message={message()}
                      displayText={presentation()?.displayText}
                      comments={presentation()?.comments}
                      references={presentation()?.references}
                      voice={presentation()?.voice}
                      historicalAgent={context()?.agent ?? ""}
                      historicalModel={context()?.model ?? { id: "", providerID: "" }}
                      actions={input.actions}
                      run={joined()}
                      fold={input.foldUserMessages?.()}
                      expanded={input.disclosure.value(`user-expanded:${message().id}`)}
                      onExpandedChange={(open) => input.disclosure.set(`user-expanded:${message().id}`, open)}
                    />
                  </div>
                </div>
              )
            }}
          </Show>
        </Frame>
      )
    }

    if (Predicate.isTagged(row(), "PeerMessage")) {
      const current = () => {
        const value = row()

        if (!Predicate.isTagged(value, "PeerMessage")) throw new Error("Expected a peer-message timeline row")

        return value
      }

      return (
        <Frame row={current()} grow={arriving(current().userMessageID)}>
          <PeerMessage messageID={current().messageID} />
        </Frame>
      )
    }

    if (Predicate.isTagged(row(), "Shell")) {
      const current = () => {
        const value = row()

        if (!Predicate.isTagged(value, "Shell")) throw new Error("Expected a shell timeline row")

        return value
      }

      return (
        <Frame row={current()}>
          <Shell messageID={current().messageID} />
        </Frame>
      )
    }

    if (Predicate.isTagged(row(), "Notice")) {
      const current = () => {
        const value = row()

        if (!Predicate.isTagged(value, "Notice")) throw new Error("Expected a notice timeline row")

        return value
      }

      return (
        <Frame row={current()}>
          <Notice messageID={current().messageID} />
        </Frame>
      )
    }

    if (Predicate.isTagged(row(), "TurnDivider")) {
      const current = () => {
        const value = row()

        if (!Predicate.isTagged(value, "TurnDivider")) throw new Error("Expected a turn-divider timeline row")

        return value
      }

      return (
        <Frame row={current()}>
          <div data-slot="session-turn-message-container" class={`w-full ${padding()}`}>
            <div data-slot="session-turn-compaction">
              <div class="py-2">
                <TimelineSeparator label={i18n.t("ui.message.interrupted")} />
              </div>
            </div>
          </div>
        </Frame>
      )
    }

    if (Predicate.isTagged(row(), "CompactionQueued")) {
      const current = () => {
        const value = row()

        if (!Predicate.isTagged(value, "CompactionQueued")) throw new Error("Expected a queued-compaction timeline row")

        return value
      }

      const compact = input.actions?.compact

      return (
        <Frame row={current()}>
          <div data-slot="session-turn-message-container" class={`w-full ${padding()}`}>
            <div data-slot="session-turn-compaction">
              <SessionCompactionQueued
                phase={current().phase}
                trigger={current().trigger}
                estimateTokens={current().estimateTokens}
                triggerTokens={current().triggerTokens}
                ceilingTokens={current().ceilingTokens}
                onCompact={
                  compact && current().phase === "suggested"
                    ? () => void compact({ sessionID: input.sessionID() })
                    : undefined
                }
              />
            </div>
          </div>
        </Frame>
      )
    }

    if (Predicate.isTagged(row(), "AssistantPart")) {
      const current = () => {
        const value = row()

        if (!Predicate.isTagged(value, "AssistantPart")) throw new Error("Expected an assistant-part timeline row")

        return value
      }

      // Construct once per row key, not inside JSX that reruns when group refs change.
      const content = renderAssistant(current, onSizeChange)

      return (
        <Frame row={current()}>
          <div data-slot="session-turn-message-container" class={`w-full ${padding()}`}>
            <div data-slot="session-turn-assistant-content" aria-hidden={workingTurn(current().userMessageID)}>
              {content}
            </div>
          </div>
        </Frame>
      )
    }

    if (Predicate.isTagged(row(), "Thinking")) {
      const current = () => {
        const value = row()

        if (!Predicate.isTagged(value, "Thinking")) throw new Error("Expected a thinking timeline row")

        return value
      }

      const content = createMemo(() => {
        const ref = current().ref
        const content = Timeline.resolveContent(input.projection.messageByID().get(ref.messageID), ref.partID)

        return content?.type === "reasoning" ? content : undefined
      })

      return (
        <Frame row={current()}>
          <div data-slot="session-turn-message-container" class={`w-full ${padding()}`}>
            <div data-slot="session-turn-thinking-row">
              <Show when={content()}>
                {(content) => (
                  <AssistantReasoningContent
                    id={current().ref.partID}
                    content={content()}
                    streaming
                    defaultOpen={
                      input.timelineDetail
                        ? input.timelineDetail().thinking.details === "expanded"
                        : input.reasoningMode() === "full"
                    }
                    open={
                      input.disclosure.value(current().ref.partID) ??
                      (input.timelineDetail ? input.timelineDetail().thinking.details === "expanded" : undefined)
                    }
                    onOpenChange={(open) => input.disclosure.set(current().ref.partID, open)}
                    onContentRendered={onSizeChange}
                  />
                )}
              </Show>
            </div>
          </div>
        </Frame>
      )
    }

    if (Predicate.isTagged(row(), "TurnDiff")) {
      const current = () => {
        const value = row()

        if (!Predicate.isTagged(value, "TurnDiff")) throw new Error("Expected a turn-diff timeline row")

        return value
      }

      return (
        <Frame row={current()}>
          <div data-slot="session-turn-message-container" class={`w-full ${padding()}`}>
            <TurnDiff row={current()} onSizeChange={onSizeChange} />
          </div>
        </Frame>
      )
    }

    if (Predicate.isTagged(row(), "Retry")) {
      const current = () => {
        const value = row()

        if (!Predicate.isTagged(value, "Retry")) throw new Error("Expected a retry timeline row")

        return value
      }

      const status = createMemo(() => {
        const retry = (
          input.projection.assistantMessagesByParent().get(current().userMessageID) ?? emptyAssistantMessages
        ).at(-1)?.retry

        if (!retry) return input.status()

        return { type: "retry" as const, attempt: retry.attempt, message: retry.error.message, next: retry.at }
      })

      return (
        <Frame row={current()}>
          <div data-slot="session-turn-message-container" class={`w-full ${padding()}`}>
            <SessionRetry status={status()} show={input.projection.activeMessageID() === current().userMessageID} />
          </div>
        </Frame>
      )
    }

    const current = () => {
      const value = row()

      if (!Predicate.isTagged(value, "Error")) throw new Error("Expected an error timeline row")

      return value
    }

    const freeLimit = createMemo(() => {
      if (current().kind !== "free-limit") return
      const at = current().resetAt

      if (at === undefined || !Number.isFinite(at) || Number.isNaN(new Date(at).getTime()))
        return i18n.t("ui.sessionTurn.freeLimit.title")

      const now = Date.now()
      const sameDay = new Date(at).toDateString() === new Date(now).toDateString() && at - now < 86_400_000

      const time = new Intl.DateTimeFormat(
        i18n.locale(),
        sameDay ? { timeStyle: "short" } : { dateStyle: "medium", timeStyle: "short" },
      ).format(at)

      return i18n.t("ui.sessionTurn.freeLimit.lineReset", { time })
    })

    // A budget pauses paid models, or only the ones it covers, until it resets at the start of a month.
    const budgetLimit = createMemo(() => {
      const kind = current().kind

      if (kind !== "budget-limit" && kind !== "model-budget-limit") return
      const at = current().resetAt

      const time =
        at === undefined
          ? undefined
          : new Intl.DateTimeFormat(i18n.locale(), {
              month: "short",
              day: "numeric",
              hour: "numeric",
              minute: "2-digit",
              timeZoneName: "shortOffset",
            }).format(at)

      if (kind === "budget-limit")
        return {
          text: time
            ? i18n.t("ui.sessionTurn.budgetLimit.all", { time })
            : i18n.t("ui.sessionTurn.budgetLimit.allUntilReset"),
          action: i18n.t("ui.sessionTurn.budgetLimit.useFreeModel"),
        }

      return {
        text: time
          ? i18n.t("ui.sessionTurn.budgetLimit.models", { time })
          : i18n.t("ui.sessionTurn.budgetLimit.modelsUntilReset"),
        action: i18n.t("ui.sessionTurn.budgetLimit.switchModel"),
      }
    })

    return (
      <Frame row={current()}>
        <div data-slot="session-turn-message-container" class={`w-full ${padding()}`}>
          <Switch fallback={<SessionError message={current().text} />}>
            <Match when={budgetLimit()}>
              {(notice) => (
                <div
                  role="status"
                  class="text-[13px] font-[440] leading-text-compact tracking-[-0.04px] text-v2-text-text-muted"
                >
                  {notice().text}
                  <Show when={input.actions?.chooseModel}>
                    {(choose) => (
                      <>
                        <span aria-hidden="true">{" · "}</span>
                        <button
                          type="button"
                          class="font-[530] text-v2-text-text-base hover:underline focus-visible:underline focus-visible:outline-none"
                          onClick={() => choose()()}
                        >
                          {notice().action}
                        </button>
                      </>
                    )}
                  </Show>
                </div>
              )}
            </Match>
            <Match when={freeLimit()}>
              {(text) => (
                <div
                  role="status"
                  class="text-[13px] font-[440] leading-text-compact tracking-[-0.04px] text-v2-text-text-muted"
                >
                  {text()}
                </div>
              )}
            </Match>
          </Switch>
        </div>
      </Frame>
    )
  }

  function Row(props: { row: Accessor<TimelineRow.TimelineRow>; onSizeChange?: () => void }) {
    return (
      <Show when={TimelineRow.key(props.row())} keyed>
        {(_key) => render(props.row, props.onSizeChange)}
      </Show>
    )
  }

  return { Row }
}

function capitalizeAgent(agent: string) {
  return agent.slice(0, 1).toUpperCase() + agent.slice(1)
}

function outcomeNoticeIcon(outcome: Outcome) {
  if (outcome === "error") return "warning"

  if (outcome === "cancelled") return "circle-ban-sign"
}

function outcomeNoticeColor(outcome: Outcome | undefined) {
  if (!outcome) return ""

  if (outcome === "error") return "text-v2-state-fg-danger"

  if (outcome === "cancelled")
    return "text-[var(--v2-yellow-900)] [[data-color-scheme=dark]_&]:text-v2-state-fg-warning"

  return "text-v2-text-text-code-accent"
}

function outcomeNoticeKey(state: string | undefined) {
  if (state === "error") return "ui.sessionTimeline.notice.failed"

  if (state === "cancelled") return "ui.sessionTimeline.notice.cancelled"

  return "ui.sessionTimeline.notice.finished"
}

/** Grows a row from nothing to its natural height, clipping its content on the way. Rows below follow its edge. */
function growIn(el: HTMLElement) {
  if (prefersReducedMotion()) return
  const spring = Spring.fromDuration(0, { duration: 0.2, bounce: 0 })
  const content = el.firstElementChild
  let natural = 0
  el.style.height = "0px"
  el.style.overflow = "clip"
  el.toggleAttribute("data-timeline-grow", true)

  const remove = frameLoop.addMeasured(
    () => {
      natural = content instanceof HTMLElement ? content.offsetHeight : 0
    },
    (dt) => {
      if (!el.isConnected) return true
      spring.set(natural)
      const height = spring.step(dt)

      if (!spring.resting) {
        el.style.height = `${height}px`

        return true
      }

      el.style.height = ""
      el.style.overflow = ""
      el.toggleAttribute("data-timeline-grow", false)

      return false
    },
  )

  onCleanup(remove)
}
