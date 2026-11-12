import { describe, expect, test } from "bun:test"
import {
  asCount,
  asText,
  parseGoal,
  parseLaneStatuses,
  parseLaneTicket,
  parseObserverRepaired,
  parseObserverRepairs,
  parseObserverState,
  parsePeerMessage,
  parseSubagentTree,
} from "./agent-wire"

describe("wire value helpers", () => {
  test("accepts only non-empty strings and finite numbers", () => {
    expect(asText("value")).toBe("value")
    expect(asText("")).toBeUndefined()
    expect(asText(1)).toBeUndefined()
    expect(asText(null)).toBeUndefined()

    expect(asCount(0)).toBe(0)
    expect(asCount(12.5)).toBe(12.5)
    expect(asCount(Number.NaN)).toBeUndefined()
    expect(asCount(Number.POSITIVE_INFINITY)).toBeUndefined()
    expect(asCount("12")).toBeUndefined()
  })
})

describe("parseGoal", () => {
  test("reads the documented fields and ignores unknown ones", () => {
    expect(
      parseGoal({
        id: "gol_1",
        rootID: "ses_root",
        objective: "ship it",
        criteria: ["tests", 2, "review"],
        status: "paused",
        rounds: 3,
        stalls: 1,
        evidence: ["msg_1", null],
        model: "test/alpha",
        surprise: { nested: true },
      }),
    ).toEqual({
      id: "gol_1",
      rootID: "ses_root",
      objective: "ship it",
      criteria: ["tests", "review"],
      status: "paused",
      rounds: 3,
      stalls: 1,
      evidence: ["msg_1"],
      model: "test/alpha",
    })
  })

  test("falls back to active/no rounds and an empty root", () => {
    expect(parseGoal({ id: "gol_1", objective: "go" })).toEqual({
      id: "gol_1",
      rootID: "",
      objective: "go",
      criteria: [],
      status: "active",
      rounds: 0,
      stalls: 0,
      evidence: [],
      model: undefined,
    })
  })

  test("returns undefined when the goal cannot be identified", () => {
    expect(parseGoal(undefined)).toBeUndefined()
    expect(parseGoal(null)).toBeUndefined()
    expect(parseGoal([])).toBeUndefined()
    expect(parseGoal("goal")).toBeUndefined()
    expect(parseGoal({ objective: "no id" })).toBeUndefined()
    expect(parseGoal({ id: "gol_1" })).toBeUndefined()
    expect(parseGoal({ id: "", objective: "" })).toBeUndefined()
  })
})

describe("parseObserverRepairs", () => {
  test("maps targetID before target and takes created from row.time.created", () => {
    expect(
      parseObserverRepairs([
        {
          id: "rep_1",
          kind: "tool",
          reason: "missing output",
          targetID: "call_1",
          target: "call_ignored",
          attempt: 2,
          time: { created: 1_700_000_000_000 },
          unknown: "ignored",
        },
        { kind: "message", target: "call_2" },
      ]),
    ).toEqual([
      {
        id: "rep_1",
        kind: "tool",
        reason: "missing output",
        target: "call_1",
        attempt: 2,
        created: 1_700_000_000_000,
      },
      { id: undefined, kind: "message", reason: "", target: "call_2", attempt: 0, created: undefined },
    ])
  })

  test("drops rows without a kind and non-array bodies", () => {
    expect(parseObserverRepairs([{ target: "call_1" }, "row", null, { kind: "tool" }])).toEqual([
      { id: undefined, kind: "tool", reason: "", target: "", attempt: 0, created: undefined },
    ])
    expect(parseObserverRepairs(undefined)).toEqual([])
    expect(parseObserverRepairs({ kind: "tool" })).toEqual([])
  })
})

describe("parseObserverRepaired", () => {
  test("reads the event payload and prefers target over targetID", () => {
    expect(
      parseObserverRepaired({ kind: "tool", reason: "retry", target: "call_1", targetID: "call_2", attempt: 1 }),
    ).toEqual({ kind: "tool", reason: "retry", target: "call_1", attempt: 1 })
    expect(parseObserverRepaired({ kind: "message", targetID: "call_2" })).toEqual({
      kind: "message",
      reason: "",
      target: "call_2",
      attempt: 0,
    })
  })

  test("returns undefined when the payload has no kind", () => {
    expect(parseObserverRepaired({ target: "call_1" })).toBeUndefined()
    expect(parseObserverRepaired(null)).toBeUndefined()
    expect(parseObserverRepaired([])).toBeUndefined()
  })
})

describe("parseObserverState", () => {
  test("maps the root budget fields", () => {
    expect(parseObserverState({ budget: { rootUsed: 4, rootLimit: 10, exhausted: true, other: 1 } })).toEqual({
      budget: { used: 4, limit: 10, exhausted: true },
    })
    expect(parseObserverState({ budget: { root_used: 7, root_limit: 12, exhausted: true } })).toEqual({
      budget: { used: 7, limit: 12, exhausted: true },
    })
    expect(parseObserverState({ budget: { rootUsed: 0, rootLimit: 0 } })).toEqual({
      budget: { used: 0, limit: 0, exhausted: false },
    })
  })

  test("omits a missing or malformed budget without throwing", () => {
    expect(parseObserverState({})).toEqual({ budget: undefined })
    expect(parseObserverState({ budget: null })).toEqual({ budget: undefined })
    expect(parseObserverState({ budget: [] })).toEqual({ budget: undefined })
    expect(parseObserverState("state")).toEqual({})
  })
})

describe("parseSubagentTree", () => {
  const node = {
    id: "ses_child",
    parentID: "ses_root",
    agent: "plan",
    model: "test/alpha",
    state: "busy",
    depth: 1,
    role: "subagent",
    spawnAllowed: true,
    childCount: 2,
    unknown: "ignored",
  }

  test("parses nodes and ignores unknown fields", () => {
    expect(
      parseSubagentTree({ root: "ses_root", nodes: [node, { id: "ses_leaf" }], unknown: true }),
    ).toEqual({
      root: "ses_root",
      nodes: [
        {
          id: "ses_child",
          parentID: "ses_root",
          agent: "plan",
          model: "test/alpha",
          state: "busy",
          depth: 1,
          role: "subagent",
          spawnAllowed: true,
          childCount: 2,
        },
        {
          id: "ses_leaf",
          parentID: undefined,
          agent: "build",
          model: undefined,
          state: "idle",
          depth: 0,
          role: "primary",
          spawnAllowed: false,
          childCount: 0,
        },
      ],
    })
  })

  test("never invents activity for an unknown state", () => {
    const tree = parseSubagentTree({ root: "ses_root", nodes: [{ id: "ses_child", state: "thinking" }] })
    expect(tree?.nodes[0]?.state).toBe("idle")
  })

  test("keeps every documented state", () => {
    const states = ["busy", "idle", "completed", "suspended", "failed", "interrupted", "archived"] as const
    const tree = parseSubagentTree({ root: "ses_root", nodes: states.map((state, index) => ({ id: `ses_${index}`, state })) })
    expect(tree?.nodes.map((item) => item.state)).toEqual([...states])
  })

  test("drops unreadable nodes and requires a root", () => {
    expect(parseSubagentTree({ root: "ses_root", nodes: [null, "node", { agent: "plan" }, { id: "" }] })).toEqual({
      root: "ses_root",
      nodes: [],
    })
    expect(parseSubagentTree({ root: "ses_root" })).toEqual({ root: "ses_root", nodes: [] })
    expect(parseSubagentTree({ nodes: [node] })).toBeUndefined()
    expect(parseSubagentTree(undefined)).toBeUndefined()
    expect(parseSubagentTree([])).toBeUndefined()
  })
})

describe("parseLaneStatuses", () => {
  test("parses known lanes and drops rows without one", () => {
    expect(
      parseLaneStatuses([
        { lane: "cargo", concurrency: 2, running: 1, queued: 3, extra: "ignored" },
        { running: 1 },
        null,
      ]),
    ).toEqual([{ lane: "cargo", concurrency: 2, running: 1, queued: 3 }])
    expect(parseLaneStatuses(undefined)).toEqual([])
  })
})

describe("parseLaneTicket", () => {
  test("unwraps the nested ticket and reads the lane from the envelope", () => {
    expect(
      parseLaneTicket({
        lane: "cargo",
        ticket: {
          id: "tkt_1",
          command: "cargo test",
          state: "running",
          queuedMs: 1500,
          ownerSession: "ses_owner",
          lane: "ignored",
          unknown: 1,
        },
        sessionID: "ses_envelope",
      }),
    ).toEqual({
      id: "tkt_1",
      lane: "cargo",
      command: "cargo test",
      owner: "ses_envelope",
      state: "running",
      queued: 1500,
    })
  })

  test("accepts a flat ticket and the ticket's own lane and owner", () => {
    expect(
      parseLaneTicket({ id: "tkt_1", lane: "node", command: "bun test", state: "queued", ownerSession: "ses_1" }),
    ).toEqual({ id: "tkt_1", lane: "node", command: "bun test", owner: "ses_1", state: "queued", queued: 0 })
  })

  test("reads rust-style tagged state objects", () => {
    expect(parseLaneTicket({ id: "tkt_1", lane: "cargo", state: { state: "failed" } })?.state).toBe("failed")
  })

  test("falls back to queued for an unknown state and requires id plus lane", () => {
    expect(parseLaneTicket({ id: "tkt_1", lane: "cargo", state: "paused" })?.state).toBe("queued")
    expect(parseLaneTicket({ id: "tkt_1", state: "queued" })).toBeUndefined()
    expect(parseLaneTicket({ lane: "cargo", ticket: { state: "queued" } })).toBeUndefined()
    expect(parseLaneTicket(undefined)).toBeUndefined()
    expect(parseLaneTicket([])).toBeUndefined()
  })
})

describe("parsePeerMessage", () => {
  const target = "ses_child"

  test("accepts every documented sender alias", () => {
    const aliases = ["from", "fromID", "fromSessionID", "sender", "source"] as const
    for (const alias of aliases) {
      const message = parsePeerMessage({ sessionID: target, text: "hello", [alias]: "ses_peer", created: 5 }, target)
      expect(message).toEqual({
        id: "ses_peer:5",
        from: "ses_peer",
        agent: undefined,
        kind: undefined,
        text: "hello",
        created: 5,
      })
    }
  })

  test("prefers the aliases in the documented order and defaults to peer", () => {
    expect(
      parsePeerMessage(
        { sessionID: target, text: "hello", from: "ses_a", fromID: "ses_b", sender: "ses_c", created: 1 },
        target,
      )?.from,
    ).toBe("ses_a")
    expect(parsePeerMessage({ sessionID: target, text: "hello", created: 1 }, target)?.from).toBe("peer")
  })

  test("rejects a message addressed to another session or without a target", () => {
    expect(parsePeerMessage({ sessionID: "ses_other", text: "hello" }, target)).toBeUndefined()
    expect(parsePeerMessage({ text: "hello" }, target)).toBeUndefined()
    expect(parsePeerMessage({ sessionID: target, text: "" }, target)).toBeUndefined()
    expect(parsePeerMessage(null, target)).toBeUndefined()
  })

  test("matches target, targetID and to as the destination", () => {
    for (const key of ["sessionID", "target", "targetID", "to"] as const) {
      expect(parsePeerMessage({ [key]: target, text: "hello", created: 2 }, target)?.text).toBe("hello")
    }
  })

  test("reads text from message, payload.text and the message id/time wrappers", () => {
    expect(
      parsePeerMessage(
        {
          target,
          message: "body",
          messageID: "msg_1",
          agent: "plan",
          kind: "question",
          time: { created: 42 },
        },
        target,
      ),
    ).toEqual({ id: "msg_1", from: "peer", agent: "plan", kind: "question", text: "body", created: 42 })
    expect(parsePeerMessage({ target, payload: { text: "nested" }, created: 3 }, target)?.text).toBe("nested")
    expect(parsePeerMessage({ target, text: "typed", id: "row_1", created: 4 }, target)?.id).toBe("row_1")
  })

  test("keeps only the known message fields", () => {
    expect(
      parsePeerMessage(
        { target, text: "hello", from: "ses_peer", created: 5, unknown: "ignored", payload: "ignored" },
        target,
      ),
    ).toEqual({ id: "ses_peer:5", from: "ses_peer", agent: undefined, kind: undefined, text: "hello", created: 5 })
  })
})
