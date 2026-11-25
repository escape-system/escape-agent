import { afterEach, describe, expect, mock, test } from "bun:test"

// `bun test --conditions=solid` — the documented unit command for this package — resolves
// `solid-js` to `dist/server.js`, the SSR build, where `onMount` is literally
// `function onMount(fn) {}`. Component effects therefore never run, so flushing microtasks or
// macrotasks cannot make these DOM tests observe the component. Load the client build once and
// install it as `solid-js` for this file's module registry (running under `--isolate`, so no
// other test file is affected) to get the real `onMount`/`onCleanup` semantics.
// The specifier lives in a variable so `tsc` does not require declarations for the deep path.
const clientSolidSpecifier = "solid-js/dist/solid.js"
const clientSolid = await import(clientSolidSpecifier)
mock.module("solid-js", () => clientSolid)

const { createRoot } = await import("solid-js")
const { isEditableTarget, KEYBOARD_MIN_COVERAGE, KeyboardInsets, keyboardInset } = await import("./keyboard")

type Listener = (event: Event) => void

/** A `visualViewport` stand-in: mutable metrics plus a real listener registry. */
function fakeViewport(height: number) {
  const listeners = new Map<string, Set<Listener>>()

  return {
    height,
    offsetTop: 0,
    scale: 1,
    addEventListener(type: string, listener: Listener) {
      const set = listeners.get(type) ?? new Set<Listener>()
      set.add(listener)
      listeners.set(type, set)
    },
    removeEventListener(type: string, listener: Listener) {
      listeners.get(type)?.delete(listener)
    },
    dispatch(type: string) {
      for (const listener of [...(listeners.get(type) ?? [])]) listener(new Event(type))
    },
    count() {
      let total = 0
      for (const set of listeners.values()) total += set.size
      return total
    },
  }
}

/** The style surface the component writes, in memory. */
function fakeTarget(clientHeight = 800) {
  const values = new Map<string, string>()
  const target = {
    clientHeight,
    style: {
      setProperty(property: string, value: string) {
        values.set(property, value)
      },
      removeProperty(property: string) {
        values.delete(property)
        return ""
      },
    },
  }
  return { target: target as unknown as HTMLElement, values, targetRef: target }
}

let active: Element | null = null

function install() {
  Object.defineProperty(document, "activeElement", { configurable: true, get: () => active })
}

function editor() {
  const input = document.createElement("input")
  document.body.append(input)
  return input
}

async function mount(options: {
  target?: HTMLElement
  viewport?: VisualViewport | null
}) {
  let dispose = () => {}
  createRoot((stop) => {
    dispose = stop
    KeyboardInsets(options)
  })
  // With the client build `onMount` runs inside `createRoot`, but flush a macrotask so the
  // harness can never observe a half-mounted component if effect scheduling changes.
  await new Promise((resolve) => setTimeout(resolve, 0))
  return dispose
}

afterEach(() => {
  active = null
  Reflect.deleteProperty(document, "activeElement")
  document.body.replaceChildren()
})

describe("isEditableTarget", () => {
  test("accepts text entry and rejects everything else", () => {
    const input = document.createElement("input")
    const readonly = document.createElement("input")
    readonly.setAttribute("readonly", "")
    const textarea = document.createElement("textarea")
    const button = document.createElement("button")

    expect(isEditableTarget(input)).toBe(true)
    expect(isEditableTarget(textarea)).toBe(true)
    expect(isEditableTarget(readonly)).toBe(false)
    expect(isEditableTarget(button)).toBe(false)
    expect(isEditableTarget(null)).toBe(false)
  })
})

describe("keyboardInset", () => {
  test("reports the covered height when a keyboard shrinks the visual viewport", () => {
    expect(keyboardInset({ layoutHeight: 800, height: 500 })).toBe(300)
  })

  test("returns zero for no coverage, browser chrome, and the exact threshold", () => {
    expect(keyboardInset({ layoutHeight: 800, height: 800 })).toBe(0)
    expect(keyboardInset({ layoutHeight: 800, height: 760 })).toBe(0)
    expect(keyboardInset({ layoutHeight: 800, height: 800 - KEYBOARD_MIN_COVERAGE })).toBe(0)
    expect(keyboardInset({ layoutHeight: 500, height: 700 })).toBe(0)
  })

  test("ignores pinch zoom because the visual viewport scales with it", () => {
    expect(keyboardInset({ layoutHeight: 800, height: 800, scale: 2 })).toBe(0)
    expect(keyboardInset({ layoutHeight: 800, height: 790, scale: 1.5 })).toBe(0)
  })

  test("subtracts the visual viewport offset", () => {
    expect(keyboardInset({ layoutHeight: 800, height: 500, offsetTop: 120 })).toBe(180)
  })

  test("rounds subpixel coverage and treats unknown metrics as no keyboard", () => {
    expect(keyboardInset({ layoutHeight: 800, height: 500.4 })).toBe(300)
    expect(keyboardInset({ layoutHeight: Number.NaN, height: 500 })).toBe(0)
    expect(keyboardInset({ layoutHeight: 800, height: Number.NaN })).toBe(0)
    expect(keyboardInset({ layoutHeight: 800, height: 500, scale: Number.NaN })).toBe(300)
  })
})

describe("KeyboardInsets", () => {
  test("opens, resizes and closes --keyboard-inset from the visual viewport", async () => {
    install()
    const viewport = fakeViewport(800)
    const { target, values } = fakeTarget(800)
    active = editor()

    const dispose = await mount({ target, viewport: viewport as unknown as VisualViewport })
    expect(values.get("--keyboard-inset")).toBe("0px")

    viewport.height = 460
    viewport.dispatch("resize")
    expect(values.get("--keyboard-inset")).toBe("340px")
    expect(values.get("--safe-area-inset-bottom")).toBe("0px")

    viewport.height = 780
    viewport.dispatch("resize")
    expect(values.get("--keyboard-inset")).toBe("0px")
    expect(values.has("--safe-area-inset-bottom")).toBe(false)

    // The scroll event matters on iOS: closing the keyboard can move offsetTop only.
    viewport.height = 500
    viewport.offsetTop = 20
    viewport.dispatch("scroll")
    expect(values.get("--keyboard-inset")).toBe("280px")

    dispose()
    expect(values.has("--keyboard-inset")).toBe(false)
    expect(values.has("--safe-area-inset-bottom")).toBe(false)
    expect(viewport.count()).toBe(0)
  })

  test("focus changes open and close the inset without a resize", async () => {
    install()
    const viewport = fakeViewport(800)
    const { target, values } = fakeTarget(800)
    const input = editor()
    active = null

    const dispose = await mount({ target, viewport: viewport as unknown as VisualViewport })

    active = input
    viewport.height = 520
    document.dispatchEvent(new Event("focusin"))
    expect(values.get("--keyboard-inset")).toBe("280px")

    active = null
    viewport.height = 800
    document.dispatchEvent(new Event("focusout"))
    expect(values.get("--keyboard-inset")).toBe("0px")
    dispose()
  })

  test("ignores viewport shrink while no editor is focused", async () => {
    install()
    const viewport = fakeViewport(800)
    const { target, values } = fakeTarget(800)
    active = document.body

    const dispose = await mount({ target, viewport: viewport as unknown as VisualViewport })
    viewport.height = 300
    viewport.dispatch("resize")
    viewport.dispatch("scroll")
    expect(values.get("--keyboard-inset")).toBe("0px")
    dispose()
  })

  test("falls back to zero without a visual viewport", async () => {
    install()
    const { target, values } = fakeTarget(800)
    active = editor()

    const dispose = await mount({ target, viewport: null })
    expect(values.get("--keyboard-inset")).toBe("0px")
    expect(values.has("--safe-area-inset-bottom")).toBe(false)
    dispose()
  })

  test("uses the target's layout height over the window", async () => {
    install()
    const viewport = fakeViewport(800)
    const { target, values } = fakeTarget(600)
    active = editor()

    const dispose = await mount({ target, viewport: viewport as unknown as VisualViewport })
    expect(values.get("--keyboard-inset")).toBe("0px")
    viewport.height = 420
    viewport.dispatch("resize")
    expect(values.get("--keyboard-inset")).toBe("180px")
    dispose()
  })
})
