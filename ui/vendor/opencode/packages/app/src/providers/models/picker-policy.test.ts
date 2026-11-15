import { describe, expect, test } from "bun:test"
import {
  chainIndexOf,
  comparePickerModels,
  effectiveAgentChain,
  favoriteRefs,
  modelReference,
  normalizeModelRef,
  pickerVisible,
  type PickerOrder,
} from "./policy"

const entry = (visible = true, favorite = false) => ({ visible, favorite })

const ref = (providerID: string, id: string) => modelReference({ providerID, id })

describe("references", () => {
  test("drops a variant suffix, lowercases and folds dots", () => {
    expect(normalizeModelRef("Anthropic/Claude.Sonnet-4#high")).toBe("anthropic/claude-sonnet-4")
    expect(normalizeModelRef("  openai/gpt-5.6  ")).toBe("openai/gpt-5-6")
    expect(normalizeModelRef("a/1")).toBe("a/1")
  })

  test("a #variant chain entry matches its base model", () => {
    expect(chainIndexOf(["a/1#high"], ref("a", "1"))).toBe(0)
    expect(chainIndexOf(["a/1"], ref("a", "1"))).toBe(0)
    expect(chainIndexOf(["a/1"], ref("a", "2"))).toBe(-1)
    expect(chainIndexOf([], ref("a", "1"))).toBe(-1)
  })

  test("an agent chain wins over the global one, and an empty one restricts nothing", () => {
    expect(effectiveAgentChain({ build: ["a/1"] }, "build", ["b/2"])).toEqual(["a/1"])
    expect(effectiveAgentChain({ build: [] }, "build", ["b/2"])).toEqual([])
    expect(effectiveAgentChain({ build: ["a/1"] }, "plan", ["b/2"])).toEqual(["b/2"])
    expect(effectiveAgentChain(undefined, "build", ["b/2"])).toEqual(["b/2"])
  })
})

describe("picker visibility", () => {
  test("without a registry the legacy store rule answers", () => {
    expect(pickerVisible({ modelRef: "a/1", legacyVisible: true })).toBe(true)
    expect(pickerVisible({ modelRef: "a/1", legacyVisible: false })).toBe(false)
    expect(pickerVisible({ modelRef: "a/1" })).toBe(true)
  })

  test("every registry model the config does not hide is offered", () => {
    expect(pickerVisible({ modelRef: "a/1", registryModel: entry() })).toBe(true)
    expect(pickerVisible({ modelRef: "b/2", registryModel: entry() })).toBe(true)
  })

  test("a model absent from a non-empty chain is still visible", () => {
    // The live single-entry chain must not shrink 692 models to one.
    const chain = ["a/1", "b/2#high"]

    expect(pickerVisible({ modelRef: "a/1", registryModel: entry() })).toBe(true)
    expect(pickerVisible({ modelRef: "b/2", registryModel: entry() })).toBe(true)
    expect(pickerVisible({ modelRef: "c/3", registryModel: entry() })).toBe(true)
    expect(chainIndexOf(chain, ref("c", "3"))).toBe(-1)
  })

  test("hidden models disappear, from the registry and from the legacy set", () => {
    expect(pickerVisible({ modelRef: "a/1", registryModel: entry(false) })).toBe(false)
    expect(
      pickerVisible({ modelRef: "a/1", hiddenSet: new Set([normalizeModelRef("a/1")]), legacyVisible: true }),
    ).toBe(false)
    // The registry wins over a stale legacy answer.
    expect(pickerVisible({ modelRef: "a/1", registryModel: entry(false), legacyVisible: true })).toBe(false)
  })

  test("hidden is the only exclusion besides provider connectivity", () => {
    expect(pickerVisible({ modelRef: "a/1", registryModel: entry(false) })).toBe(false)
    expect(pickerVisible({ modelRef: "a/1", legacyVisible: false })).toBe(false)
    // No chain can turn a visible model off, and none can turn a hidden one on.
    expect(pickerVisible({ modelRef: "z/9", registryModel: entry(true) })).toBe(true)
    expect(pickerVisible({ modelRef: "z/9", registryModel: entry(false) })).toBe(false)
  })

  test("a model of an unconnected provider is never offered", () => {
    expect(pickerVisible({ modelRef: "a/1", registryModel: entry(), providerConnected: false })).toBe(false)
    expect(pickerVisible({ modelRef: "a/1", legacyVisible: true, providerConnected: false })).toBe(false)
  })
})

describe("picker ordering", () => {
  const order = (name: string, favorite: boolean, chainIndex = -1): PickerOrder => ({
    name,
    ref: `x/${name}`,
    favorite,
    chainIndex,
  })

  test("favorites lead, then the chain order, then the name", () => {
    const items = [order("Zed", false, 2), order("Beta", true, -1), order("Alpha", false, 0), order("Gamma", false, -1)]

    expect([...items].sort(comparePickerModels).map((item) => item.name)).toEqual([
      "Beta",
      "Alpha",
      "Zed",
      "Gamma",
    ])
  })

  test("models outside the chain follow every chained model, whatever their name", () => {
    const items = [order("aaa", false, -1), order("zzz", false, 0)]

    expect([...items].sort(comparePickerModels).map((item) => item.name)).toEqual(["zzz", "aaa"])
  })

  test("an unchained list sorts by name only", () => {
    const items = [order("Cee", false, -1), order("Aye", false, -1), order("Bee", false, -1)]

    expect([...items].sort(comparePickerModels).map((item) => item.name)).toEqual(["Aye", "Bee", "Cee"])
  })

  test("a name tie keeps a stable order by ref", () => {
    const items = [
      { name: "same", ref: "b/2", favorite: false, chainIndex: -1 },
      { name: "same", ref: "a/1", favorite: false, chainIndex: -1 },
    ]

    expect([...items].sort(comparePickerModels).map((item) => item.ref)).toEqual(["a/1", "b/2"])
  })

  test("favoriteRefs normalizes the registry's favorites", () => {
    const models = [
      { providerID: "Anthropic", id: "Claude.Sonnet-4", favorite: true },
      { providerID: "openai", id: "gpt-5.6", favorite: false },
    ]

    expect([...favoriteRefs(models)]).toEqual(["anthropic/claude-sonnet-4"])
    expect(favoriteRefs([])).toEqual(new Set())
  })
})
