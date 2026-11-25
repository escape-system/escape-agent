import type { Page } from "@playwright/test"
import type { OpenCodeEvent } from "@opencode/client/promise"
import { SERVER, sessionHref } from "../utils/app"
import { mockWorkspace, type WorkspaceInput } from "../utils/workspace"
import { expectSessionReady } from "../utils/waits"

/**
 * Verification-only harness shim (drafted, not yet executed).
 *
 * The session screen always queries the agentic extension routes
 * (`/api/experimental/{goal,lane,observer,observer/repairs,subagent/tree}`), but
 * `e2e/utils/mock-server.ts` has no route for any of them: it answers 501 and
 * then throws "Unmocked API request", failing the test regardless of the
 * behavior under test. These stubs answer them locally instead.
 *
 * Ordering matters. Playwright evaluates route handlers in reverse registration
 * order, so these MUST be registered after `mockOpenCodeServer`'s `**\/api/**`
 * catch-all, and the navigation must happen after that. `prepareSession` owns
 * the `page.goto` for exactly that reason; the harness helpers that navigate
 * (`setupTimeline`, `openSession`) do so before a caller could add stubs.
 */
export async function mockAgenticRoutes(
  page: Page,
  input: {
    tree?: (sessionID: string) => unknown
    repairs?: unknown[]
    observer?: unknown
    goal?: unknown
    lanes?: unknown[]
  } = {},
) {
  const answer = (value: unknown) => ({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({ data: value }),
  })

  await page.route("**/api/experimental/goal**", (route) => route.fulfill(answer(input.goal ?? null)))
  await page.route("**/api/experimental/lane**", (route) => route.fulfill(answer(input.lanes ?? [])))
  // Registered before `observer/repairs` so the narrower pattern wins.
  await page.route("**/api/experimental/observer**", (route) => route.fulfill(answer(input.observer ?? {})))
  await page.route("**/api/experimental/observer/repairs**", (route) =>
    route.fulfill(answer(input.repairs ?? [])),
  )
  await page.route("**/api/experimental/subagent/tree**", (route) => {
    const id = new URL(route.request().url()).searchParams.get("sessionID") ?? ""
    return route.fulfill(answer(input.tree?.(id) ?? { root: id, nodes: [] }))
  })
}

export type PreparedSession = {
  /** Sends events on this origin's mock stream; buffered until the app connects. */
  pushEvents: (events: readonly OpenCodeEvent[]) => Promise<void>
  sessionID: string
}

/**
 * `mockWorkspace` + agentic stubs + navigation. Session data, tabs and the
 * used-project entry come from the harness, so a spec declares only what it
 * asserts on.
 */
export async function prepareSession(
  page: Page,
  input: {
    sessionID: string
    /** Omit for a child session, whose header is the spawning task description. */
    title?: string
    sessions: NonNullable<WorkspaceInput["sessions"]>
    pageMessages: NonNullable<WorkspaceInput["pageMessages"]>
    inbox?: WorkspaceInput["inbox"]
    sessionStatus?: WorkspaceInput["sessionStatus"]
    tree?: (sessionID: string) => unknown
    repairs?: unknown[]
    width?: number
    height?: number
  },
): Promise<PreparedSession> {
  if (input.width && input.height) await page.setViewportSize({ width: input.width, height: input.height })

  const workspace = await mockWorkspace(page, {
    name: "VerifyAgentic",
    sessions: input.sessions,
    pageMessages: input.pageMessages,
    inbox: input.inbox,
    sessionStatus: input.sessionStatus,
  })
  await mockAgenticRoutes(page, { tree: input.tree, repairs: input.repairs })

  await page.goto(sessionHref(input.sessionID, workspace.server))
  await expectSessionReady(page, {
    server: workspace.server,
    sessionID: input.sessionID,
    title: input.title ?? input.sessionID,
  })

  return { pushEvents: workspace.push, sessionID: input.sessionID }
}

/** The timeline scrollport (`<div data-slot="session-timeline-scroll">` -> scrollable region). */
export function timelineScroller(page: Page) {
  return page
    .locator('[data-slot="session-timeline-scroll"]')
    .getByRole("region", { name: "scrollable content", exact: true })
}

/** Fails when the page can scroll horizontally at the current width. */
export async function expectNoHorizontalOverflow(page: Page) {
  const result = await page.evaluate(() => ({
    width: window.innerWidth,
    document: document.documentElement.scrollWidth - window.innerWidth,
    body: document.body.scrollWidth - window.innerWidth,
  }))

  if (result.document > 1 || result.body > 1)
    throw new Error(
      `horizontal overflow at ${result.width}px: document +${result.document}px, body +${result.body}px`,
    )
}

export { SERVER }
