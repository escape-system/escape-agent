import { describe, expect, test } from "bun:test"
import { modelRef, parseAgents, parseModelRef, parseRegistry, registryPath, registryQueryKey } from "./api"

const baseModel = {
  providerID: "google",
  id: "gemini-3-pro",
  name: "Gemini 3 Pro",
  visible: true,
  favorite: false,
}

describe("parseRegistry", () => {
  test("reads the documented body and defaults the optional blocks", () => {
    const registry = parseRegistry({
      data: {
        models: [baseModel],
        chain: ["google/gemini-3-pro"],
        fallback: { on: ["rate_limit"], maxHops: 2 },
        usage: { windowHours: 120, totalCostUsd: 1.2, totalRequests: 30 },
      },
    })

    expect(registry?.models.map((model) => modelRef(model.providerID, model.id))).toEqual(["google/gemini-3-pro"])
    expect(registry?.chain).toEqual(["google/gemini-3-pro"])
    expect(registry?.chains).toEqual({})
    expect(registry?.agentNotes).toEqual({})
    expect(registry?.quota).toEqual({})
    expect(registry?.modelHealth).toEqual([])
    expect(registry?.keyHealth).toEqual([])
    expect(registry?.models[0].unavailable).toBeUndefined()
  })

  test("unwraps a bare body as well as the data envelope", () => {
    expect(parseRegistry({ models: [baseModel], chain: [] })?.models).toHaveLength(1)
    expect(parseRegistry({ data: { models: [] } })?.models).toEqual([])
    expect(parseRegistry({ data: { chain: [] } })).toBeUndefined()
    expect(parseRegistry(null)).toBeUndefined()
    expect(parseRegistry("nope")).toBeUndefined()
  })

  test("parses chains keyed by agent", () => {
    const registry = parseRegistry({
      data: { models: [baseModel], chain: [], chains: { explore: ["a/1", "b/2"], build: [] } },
    })

    expect(registry?.chains).toEqual({ explore: ["a/1", "b/2"], build: [] })
  })

  test("parses a model's temporary unavailability", () => {
    const registry = parseRegistry({
      data: {
        models: [
          {
            ...baseModel,
            unavailable: { untilMs: 1_700_000_000_000, kind: "rate_limit", message: "429 for 60s" },
          },
        ],
        chain: [],
      },
    })

    expect(registry?.models[0].unavailable).toEqual({
      untilMs: 1_700_000_000_000,
      kind: "rate_limit",
      message: "429 for 60s",
    })
  })

  test("drops an unavailability entry without kind or until", () => {
    const missingKind = parseRegistry({
      data: { models: [{ ...baseModel, unavailable: { untilMs: 1 } }], chain: [] },
    })
    const missingUntil = parseRegistry({
      data: { models: [{ ...baseModel, unavailable: { kind: "rate_limit" } }], chain: [] },
    })

    expect(missingKind?.models[0].unavailable).toBeUndefined()
    expect(missingUntil?.models[0].unavailable).toBeUndefined()
  })

  test("parses modelHealth and derives a missing reference", () => {
    const registry = parseRegistry({
      data: {
        models: [baseModel],
        chain: [],
        modelHealth: [
          {
            providerID: "anthropic",
            id: "claude-sonnet-4",
            reference: "anthropic/claude-sonnet-4",
            untilMs: 1_700_000_000_000,
            kind: "transport",
          },
          { providerID: "google", id: "gemini-3-pro", untilMs: 5, kind: "quota", message: "budget" },
          { providerID: "google", untilMs: 5, kind: "quota" },
          { providerID: "google", id: "gemini-3-pro", kind: "quota" },
          "nonsense",
        ],
      },
    })

    expect(registry?.modelHealth).toEqual([
      {
        providerID: "anthropic",
        id: "claude-sonnet-4",
        reference: "anthropic/claude-sonnet-4",
        untilMs: 1_700_000_000_000,
        kind: "transport",
      },
      { providerID: "google", id: "gemini-3-pro", reference: "google/gemini-3-pro", untilMs: 5, kind: "quota", message: "budget" },
    ])
  })

  test("parses keyHealth and omits an absent credentialID so it reads as the env key", () => {
    const registry = parseRegistry({
      data: {
        models: [baseModel],
        chain: [],
        keyHealth: [
          { providerID: "google", credentialID: "cred_1", untilMs: 1_700_000_000_000, kind: "quota", failures: 2 },
          { providerID: "anthropic", untilMs: 5, kind: "rate_limit", failures: 1 },
          { providerID: "anthropic", untilMs: 5, kind: "rate_limit" },
          { providerID: "google", kind: "quota" },
          { untilMs: 5, kind: "quota" },
          "nonsense",
        ],
      },
    })

    expect(registry?.keyHealth).toEqual([
      { providerID: "google", credentialID: "cred_1", untilMs: 1_700_000_000_000, kind: "quota", failures: 2 },
      { providerID: "anthropic", untilMs: 5, kind: "rate_limit", failures: 1 },
      // No counter on the wire still shows the entry rather than dropping it.
      { providerID: "anthropic", untilMs: 5, kind: "rate_limit", failures: 0 },
    ])
    // Absent means the provider's environment key: never an invented id.
    expect("credentialID" in registry!.keyHealth[1]).toBe(false)
  })

  test("keeps an agent note map and drops non-string values", () => {
    const registry = parseRegistry({
      data: {
        models: [baseModel],
        chain: [],
        agentNotes: { explore: { "a/1": "read-only", "b/2": 42 }, build: "nope" },
      },
    })

    expect(registry?.agentNotes).toEqual({ explore: { "a/1": "read-only" } })
  })
})

describe("parseAgents", () => {
  test("keeps id, mode, hidden and the pinned model", () => {
    const agents = parseAgents({
      data: [
        {
          id: "build",
          name: "build",
          mode: "primary",
          hidden: false,
          model: { providerID: "anthropic", id: "claude-sonnet-4" },
          request: { settings: {}, headers: {}, body: {} },
          permissions: [],
        },
        { id: "general", name: "general", mode: "all", hidden: true },
        { id: "explore", mode: "subagent", hidden: false },
      ],
    })

    expect(agents).toEqual([
      { id: "build", name: "build", mode: "primary", hidden: false, model: { providerID: "anthropic", id: "claude-sonnet-4" } },
      { id: "explore", name: "explore", mode: "subagent", hidden: false },
      { id: "general", name: "general", mode: "all", hidden: true },
    ])
  })

  test("defaults an unknown mode to all and malformed rows are skipped", () => {
    const agents = parseAgents({
      data: [{ id: "x", name: "x", mode: "weird" }, { name: "no-id" }, "nope", { id: "y", name: "y", hidden: "yes" }],
    })

    expect(agents).toEqual([
      { id: "x", name: "x", mode: "all", hidden: false },
      { id: "y", name: "y", mode: "all", hidden: false },
    ])
  })

  test("drops a partial pinned model", () => {
    const agents = parseAgents({ data: [{ id: "a", name: "a", mode: "primary", model: { providerID: "x" } }] })

    expect(agents[0].model).toBeUndefined()
  })

  test("an unparseable body is an empty list", () => {
    expect(parseAgents(undefined)).toEqual([])
    expect(parseAgents({ data: {} })).toEqual([])
  })
})

describe("registryPath / registryQueryKey", () => {
  test("asks for the sparse body on the first paint and the usage body later", () => {
    expect(registryPath(false)).toBe("/api/models/registry?usage=0")
    expect(registryPath(true)).toBe("/api/models/registry")
  })

  test("keeps one cache entry per body shape so the paints never overwrite each other", () => {
    expect(registryQueryKey("s", false)).toEqual(["s", "models-registry", "sparse"])
    expect(registryQueryKey("s", true)).toEqual(["s", "models-registry", "usage"])
    expect(registryQueryKey("s", false)).not.toEqual(registryQueryKey("s", true))
  })
})

describe("model refs", () => {
  test("round-trip provider/model", () => {
    expect(modelRef("google", "gemini-3-pro")).toBe("google/gemini-3-pro")
    expect(parseModelRef("google/gemini-3-pro")).toEqual({ providerID: "google", id: "gemini-3-pro" })
    expect(parseModelRef("openrouter/anthropic/claude")).toEqual({
      providerID: "openrouter",
      id: "anthropic/claude",
    })
    expect(parseModelRef("no-slash")).toBeUndefined()
    expect(parseModelRef("/leading")).toBeUndefined()
    expect(parseModelRef("trailing/")).toBeUndefined()
  })
})
