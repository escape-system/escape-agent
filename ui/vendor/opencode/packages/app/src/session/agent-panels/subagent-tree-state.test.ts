// Regression coverage for where the subagent strip starts open or closed.
//
// The desktop strip collapsed to a 41px pill for users whose browser still held
// the previous release's `v2` key. That release defaulted to collapsed and wrote
// `"true"` without the user asking, so honouring it hid the only desktop list of
// every subagent. The rule under test: only an explicit current-key value counts,
// and anything else (missing key, or a legacy key on its own) starts EXPANDED.
//
// The decision lives in `readSubagentTreeCollapsed`, a pure function, because
// under `--conditions=solid` the `solid-js/web` entry resolves to Solid's SERVER
// build where `render`/`createEffect` are no-ops; a rendered-Solid test here
// would pass vacuously instead of proving the branch the component reads.

import { describe, expect, test } from "bun:test"
import {
  LEGACY_SUBAGENT_TREE_STORAGE_KEY,
  SUBAGENT_TREE_STORAGE_KEY,
  readSubagentTreeCollapsed,
  writeSubagentTreeCollapsed,
} from "./subagent-tree-state"

/** The two storage methods the helpers touch, backed by a plain map. */
function fakeStorage(entries: Record<string, string> = {}) {
  const values = new Map(Object.entries(entries))

  return {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => void values.set(key, value),
    values,
  }
}

describe("readSubagentTreeCollapsed", () => {
  test("starts expanded when nothing is stored", () => {
    expect(readSubagentTreeCollapsed(fakeStorage())).toBe(false)
  })

  test("ignores a legacy collapsed value and starts expanded", () => {
    const storage = fakeStorage({ [LEGACY_SUBAGENT_TREE_STORAGE_KEY]: "true" })
    expect(readSubagentTreeCollapsed(storage)).toBe(false)
  })

  test("honours an explicit current-key collapse", () => {
    const storage = fakeStorage({ [SUBAGENT_TREE_STORAGE_KEY]: "true" })
    expect(readSubagentTreeCollapsed(storage)).toBe(true)
  })

  test("honours an explicit current-key expansion over a legacy collapse", () => {
    const storage = fakeStorage({
      [SUBAGENT_TREE_STORAGE_KEY]: "false",
      [LEGACY_SUBAGENT_TREE_STORAGE_KEY]: "true",
    })
    expect(readSubagentTreeCollapsed(storage)).toBe(false)
  })

  test("starts expanded without any storage at all", () => {
    expect(readSubagentTreeCollapsed(undefined)).toBe(false)
  })
})

describe("writeSubagentTreeCollapsed", () => {
  test("records the choice under the current key only", () => {
    const storage = fakeStorage({ [LEGACY_SUBAGENT_TREE_STORAGE_KEY]: "true" })
    writeSubagentTreeCollapsed(true, storage)

    expect(storage.getItem(SUBAGENT_TREE_STORAGE_KEY)).toBe("true")
    // The legacy key is left untouched and unread; the current key now wins.
    expect(readSubagentTreeCollapsed(storage)).toBe(true)
  })

  test("survives a round trip for both directions", () => {
    const storage = fakeStorage()
    writeSubagentTreeCollapsed(true, storage)
    expect(readSubagentTreeCollapsed(storage)).toBe(true)

    writeSubagentTreeCollapsed(false, storage)
    expect(readSubagentTreeCollapsed(storage)).toBe(false)
  })

  test("tolerates a missing storage", () => {
    expect(() => writeSubagentTreeCollapsed(true, undefined)).not.toThrow()
  })
})
