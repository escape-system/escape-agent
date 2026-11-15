import { describe, expect, test } from "bun:test"
import {
  addToChain,
  applyChainMove,
  chainCandidates,
  chainRef,
  CHAIN_SEARCH_LIMIT,
  healthRemaining,
  highlightRanges,
  isVisibleByPolicy,
  MAX_CHAIN,
  removeFromChain,
  searchKey,
  searchState,
} from "./chain"
import type { RegistryModel } from "./api"

const model = (providerID: string, id: string, extra: Partial<RegistryModel> = {}): RegistryModel => ({
  providerID,
  id,
  name: id,
  visible: true,
  favorite: false,
  ...extra,
})

describe("chain editing", () => {
  test("addToChain appends, dedups and stops at the cap", () => {
    expect(addToChain([], "a/1")).toEqual(["a/1"])
    expect(addToChain(["a/1"], "a/1")).toEqual(["a/1"])
    expect(addToChain(["a/1"], "")).toEqual(["a/1"])
    const full = Array.from({ length: MAX_CHAIN }, (_, index) => `a/${index}`)
    expect(addToChain(full, "b/2")).toHaveLength(MAX_CHAIN)
    expect(addToChain(full, "b/2")).not.toContain("b/2")
  })

  test("addToChain and removeFromChain never mutate their input", () => {
    const chain = ["a/1", "b/2"]
    expect(addToChain(chain, "c/3")).toEqual(["a/1", "b/2", "c/3"])
    expect(removeFromChain(chain, "a/1")).toEqual(["b/2"])
    expect(chain).toEqual(["a/1", "b/2"])
  })

  test("applyChainMove swaps neighbours and ignores out-of-range moves", () => {
    const chain = ["a/1", "b/2", "c/3"]
    expect(applyChainMove(chain, 0, 1)).toEqual(["b/2", "a/1", "c/3"])
    expect(applyChainMove(chain, 2, -1)).toEqual(["a/1", "c/3", "b/2"])
    expect(applyChainMove(chain, 0, -1)).toEqual(chain)
    expect(applyChainMove(chain, 2, 1)).toEqual(chain)
    expect(applyChainMove(chain, 9, 1)).toEqual(chain)
    expect(chain).toEqual(["a/1", "b/2", "c/3"])
  })
})

describe("chain candidates", () => {
  const models = [
    model("anthropic", "claude-sonnet-4", { name: "Claude Sonnet 4", variants: ["high", "thinking"] }),
    model("google", "gemini-3-pro", { name: "Gemini 3 Pro", variants: ["xhigh"] }),
    model("opencode-go", "glm-5.3-flash", { name: "GLM 5.3 Flash" }),
  ]

  test("search keeps already-added models as a disabled list instead of dropping them", () => {
    const result = chainCandidates(models, ["google/gemini-3-pro"], "gemini")
    expect(result.candidates).toEqual([])
    expect(result.alreadyAdded.map(chainRef)).toEqual(["google/gemini-3-pro"])
  })

  test("a query matching only a chained ref still returns it under alreadyAdded", () => {
    // The caller renders "already added" from this list, so it must not be blank.
    const result = chainCandidates(models, ["opencode-go/glm-5.3-flash"], "glm-5.3-flash")

    expect(result.candidates).toEqual([])
    expect(result.alreadyAdded.map(chainRef)).toEqual(["opencode-go/glm-5.3-flash"])
  })

  test("an empty query lists every model, chained ones included", () => {
    const result = chainCandidates(models, ["google/gemini-3-pro"], "")
    expect(result.alreadyAdded.map(chainRef)).toEqual(["google/gemini-3-pro"])
    expect(result.candidates.map(chainRef)).toEqual(["anthropic/claude-sonnet-4", "opencode-go/glm-5.3-flash"])
    expect(chainCandidates(models, ["google/gemini-3-pro"], "  ").candidates).toHaveLength(2)
    // No model is dropped from the result set: 3 models in, 3 rows out.
    expect(result.candidates.length + result.alreadyAdded.length).toBe(models.length)
  })

  test("a registry with models but an empty chain lists all of them as candidates", () => {
    const result = chainCandidates(models, [], "")
    expect(result.alreadyAdded).toEqual([])
    expect(result.candidates).toHaveLength(models.length)
  })

  test("an empty registry yields both lists empty", () => {
    expect(chainCandidates([], ["a/1"], "")).toEqual({ candidates: [], alreadyAdded: [] })
  })

  test("queries match name, id, provider and variants", () => {
    expect(chainCandidates(models, [], "claude sonnet").candidates.map(chainRef)).toEqual(["anthropic/claude-sonnet-4"])
    expect(chainCandidates(models, [], "opencode-go").candidates.map(chainRef)).toEqual(["opencode-go/glm-5.3-flash"])
    // "high" is a substring of Gemini's "xhigh" variant, so both match, name-ordered.
    expect(chainCandidates(models, [], "high").candidates.map(chainRef)).toEqual([
      "anthropic/claude-sonnet-4",
      "google/gemini-3-pro",
    ])
    expect(chainCandidates(models, [], "nothing-here")).toEqual({ candidates: [], alreadyAdded: [] })
  })

  test("queries match a variant name", () => {
    expect(chainCandidates(models, [], "xhigh").candidates.map(chainRef)).toEqual(["google/gemini-3-pro"])
    expect(chainCandidates(models, [], "thinking").candidates.map(chainRef)).toEqual(["anthropic/claude-sonnet-4"])
    expect(chainCandidates(models, [], "XHIGH").candidates.map(chainRef)).toEqual(["google/gemini-3-pro"])
  })

  test("a partial id with dots and hyphens matches, case- and punctuation-insensitively", () => {
    expect(chainCandidates(models, [], "5.3-flash").candidates.map(chainRef)).toEqual(["opencode-go/glm-5.3-flash"])
    expect(chainCandidates(models, [], "glm-5.3").candidates.map(chainRef)).toEqual(["opencode-go/glm-5.3-flash"])
    expect(chainCandidates(models, [], "CLAUDE.SONNET").candidates.map(chainRef)).toEqual(["anthropic/claude-sonnet-4"])
  })
})

describe("health countdown", () => {
  const now = 1_700_000_000_000

  test("rounds up to the coarsest whole unit", () => {
    expect(healthRemaining(now + 30_000, now)).toEqual({ value: 30, unit: "second" })
    expect(healthRemaining(now + 60_000, now)).toEqual({ value: 1, unit: "minute" })
    expect(healthRemaining(now + 61_000, now)).toEqual({ value: 2, unit: "minute" })
    expect(healthRemaining(now + 3_000_000, now)).toEqual({ value: 50, unit: "minute" })
    expect(healthRemaining(now + 3_600_000, now)).toEqual({ value: 1, unit: "hour" })
    expect(healthRemaining(now + 82_800_000, now)).toEqual({ value: 23, unit: "hour" })
    expect(healthRemaining(now + 86_400_000, now)).toEqual({ value: 1, unit: "day" })
    expect(healthRemaining(now + 90_000_000, now)).toEqual({ value: 2, unit: "day" })
  })

  test("an expired or bogus timestamp has nothing left to show", () => {
    expect(healthRemaining(now, now)).toBeUndefined()
    expect(healthRemaining(now - 1_000, now)).toBeUndefined()
    expect(healthRemaining(Number.NaN, now)).toBeUndefined()
    expect(healthRemaining(now + 1_000, Number.NaN)).toBeUndefined()
  })
})

describe("policy visibility", () => {
  test("an empty chain does not restrict, hidden and unavailable always exclude", () => {
    expect(isVisibleByPolicy({ hidden: false, unavailable: false }, [], "a/1")).toBe(true)
    expect(isVisibleByPolicy({ hidden: true, unavailable: false }, [], "a/1")).toBe(false)
    expect(isVisibleByPolicy({ hidden: false, unavailable: true }, [], "a/1")).toBe(false)
  })

  test("a non-empty chain is a routing preference and never an allow-list", () => {
    expect(isVisibleByPolicy({ hidden: false, unavailable: false }, ["a/1"], "a/1")).toBe(true)
    // Absent from the chain, still listed: the chain only reorders the picker.
    expect(isVisibleByPolicy({ hidden: false, unavailable: false }, ["a/1"], "b/2")).toBe(true)
    expect(isVisibleByPolicy({ hidden: true, unavailable: false }, ["b/2"], "b/2")).toBe(false)
  })
})

describe("add-model search rows", () => {
  const list = (count: number) =>
    Array.from({ length: count }, (_, index) => model("a", `m-${index}`, { name: `Model ${index}` }))
  const added = [model("z", "chained", { name: "Chained" })]

  test("candidates come first and already-added rows last, disabled", () => {
    const state = searchState({ candidates: list(3), alreadyAdded: added })

    expect(state.rows.map((row) => row.ref)).toEqual(["a/m-0", "a/m-1", "a/m-2", "z/chained"])
    expect(state.rows.filter((row) => row.disabled).map((row) => row.ref)).toEqual(["z/chained"])
    expect(state.total).toBe(4)
    expect(state.hidden).toBe(0)
    expect(state.enabled).toBe(3)
    // Nothing chosen yet means the first row is active, so Enter always has a target.
    expect(state.activeIndex).toBe(0)
    expect(state.active?.ref).toBe("a/m-0")
  })

  test("the candidate list is capped and reports what it hides", () => {
    const state = searchState({ candidates: list(40), alreadyAdded: added })

    expect(state.rows).toHaveLength(CHAIN_SEARCH_LIMIT + 1)
    expect(state.total).toBe(41)
    expect(state.hidden).toBe(40 - CHAIN_SEARCH_LIMIT)
    expect(state.enabled).toBe(CHAIN_SEARCH_LIMIT)
    expect(state.rows.at(-1)?.ref).toBe("z/chained")
    // A different cap is honoured, so the view decides how tall the list grows.
    expect(searchState({ candidates: list(40), alreadyAdded: [], limit: 3 }).rows).toHaveLength(3)
  })

  test("an out-of-range or missing active index clamps into the rendered rows", () => {
    const candidates = list(2)

    expect(searchState({ candidates, alreadyAdded: [], activeIndex: 99 }).activeIndex).toBe(1)
    expect(searchState({ candidates, alreadyAdded: [], activeIndex: -5 }).activeIndex).toBe(0)
    expect(searchState({ candidates, alreadyAdded: [], activeIndex: Number.NaN }).activeIndex).toBe(0)
    expect(searchState({ candidates: [], alreadyAdded: [], activeIndex: 3 })).toMatchObject({
      activeIndex: -1,
      active: undefined,
      rows: [],
      total: 0,
    })
  })
})

describe("add-model keyboard reducer", () => {
  const list = (count: number) =>
    Array.from({ length: count }, (_, index) => model("a", `m-${index}`, { name: `Model ${index}` }))
  const state = () => searchState({ candidates: list(3), alreadyAdded: [model("z", "chained", { name: "Chained" })] })

  test("down and up wrap over the rendered rows, already-added ones included", () => {
    const rows = state()

    expect(searchKey(rows, "next").activeIndex).toBe(1)
    expect(searchKey({ ...rows, activeIndex: 3 }, "next").activeIndex).toBe(0)
    expect(searchKey(rows, "previous").activeIndex).toBe(3)
    expect(searchKey({ ...rows, activeIndex: 1 }, "previous").activeIndex).toBe(0)
  })

  test("Home and End jump to the ends and do not wrap", () => {
    expect(searchKey({ ...state(), activeIndex: 2 }, "first").activeIndex).toBe(0)
    expect(searchKey({ ...state(), activeIndex: 0 }, "last").activeIndex).toBe(3)
  })

  test("Enter adds the active candidate", () => {
    const rows = state()

    expect(searchKey(rows, "enter")).toEqual({ activeIndex: 0, add: "a/m-0" })
    expect(searchKey({ ...rows, activeIndex: 2 }, "enter")).toEqual({ activeIndex: 2, add: "a/m-2" })
    // The appended ref really lands at the end of the chain.
    expect(addToChain(["z/chained"], "a/m-2")).toEqual(["z/chained", "a/m-2"])
  })

  test("Enter on an already-added row refuses instead of adding a duplicate", () => {
    const rows = state()
    const outcome = searchKey({ ...rows, activeIndex: 3 }, "enter")

    expect(outcome).toEqual({ activeIndex: 3, blocked: "already-added" })
    expect(outcome.add).toBeUndefined()
  })

  test("Enter refuses at the chain cap, and arrows still work there", () => {
    const full = Array.from({ length: MAX_CHAIN }, (_, index) => `a/${index}`)

    expect(searchKey(state(), "enter", MAX_CHAIN)).toEqual({ activeIndex: 0, blocked: "cap" })
    expect(searchKey(state(), "enter", MAX_CHAIN - 1)).toEqual({ activeIndex: 0, add: "a/m-0" })
    expect(searchKey(state(), "next", MAX_CHAIN).activeIndex).toBe(1)
    // addToChain agrees with the reducer: at the cap nothing is appended.
    expect(addToChain(full, "a/m-0")).toHaveLength(MAX_CHAIN)
  })

  test("an empty list has no active row and every key is a no-op", () => {
    const empty = searchState({ candidates: [], alreadyAdded: [] })

    for (const command of ["next", "previous", "first", "last", "enter"] as const) {
      expect(searchKey(empty, command)).toEqual({ activeIndex: -1 })
    }
  })
})

describe("match highlighting", () => {
  test("returns the matched span of the original text", () => {
    expect(highlightRanges("Claude Sonnet 4", "sonnet")).toEqual([{ start: 7, end: 13 }])
    expect(highlightRanges("Claude Sonnet 4", "Claude")).toEqual([{ start: 0, end: 6 }])
    expect(highlightRanges("Claude Sonnet 4", "4")).toEqual([{ start: 14, end: 15 }])
  })

  test("matches punctuation- and case-insensitively, mapping back to real offsets", () => {
    // The query is "claude.sonnet" and the value has a space where the dot is.
    expect(highlightRanges("Claude Sonnet 4", "claude.sonnet")).toEqual([{ start: 0, end: 13 }])
    expect(highlightRanges("opencode-go/glm-5.3-flash", "GLM 5.3")).toEqual([{ start: 12, end: 19 }])
  })

  test("a compact query still points at the padded value", () => {
    expect(highlightRanges("GLM 5.3 Flash", "glm53")).toEqual([{ start: 0, end: 7 }])
  })

  test("empty queries, misses and multi-script lower-casing highlight nothing", () => {
    expect(highlightRanges("Claude Sonnet 4", "")).toEqual([])
    expect(highlightRanges("Claude Sonnet 4", "   ")).toEqual([])
    expect(highlightRanges("Claude Sonnet 4", "gemini")).toEqual([])
    // "İ".toLowerCase() is two code units, so its offsets would be wrong: no mark beats a wrong mark.
    expect(highlightRanges("İstanbul", "stan")).toEqual([])
  })

  test("overlapping and repeated matches merge into flat spans", () => {
    expect(highlightRanges("flash-flash", "flash")).toEqual([{ start: 0, end: 5 }, { start: 6, end: 11 }])
    // The compact pass covers "5", the normalized pass covers "Glm"; one merged mark spans both.
    expect(highlightRanges("Glm 5.3", "glm5")).toEqual([{ start: 0, end: 5 }])
    expect(highlightRanges("aaa", "aa")).toEqual([{ start: 0, end: 2 }])
  })
})
