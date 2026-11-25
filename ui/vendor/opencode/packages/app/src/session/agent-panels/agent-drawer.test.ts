// Regression coverage for the mobile sheet's dismissal contract:
//   - Escape closes the sheet (the browser run showed it did not), and
//   - focus returns to the opener (its trigger button) afterwards.
// The behavior lives in `listenForEscape`/`closeMobilePanel`, which the drawer
// composes. Testing those directly keeps the coverage deterministic: under
// `--conditions=solid` the `solid-js/web` entry resolves to Solid's SERVER build,
// where `render`/`createEffect` are no-ops, so a rendered-Solid test here would
// pass vacuously instead of proving anything.

import { describe, expect, test } from "bun:test"
import { closeMobilePanel, listenForEscape } from "@/shell/mobile-panel-drawer"

const escape = (init: KeyboardEventInit = {}) => {
  const event = new KeyboardEvent("keydown", {
    key: "Escape",
    bubbles: true,
    cancelable: true,
    ...init,
  })
  document.dispatchEvent(event)
  return event
}

describe("listenForEscape", () => {
  test("invokes the handler on Escape even when another handler consumed it", () => {
    let calls = 0
    const detach = listenForEscape(() => calls++)
    const squatter = (event: KeyboardEvent) => {
      if (event.key === "Escape") event.preventDefault()
    }
    document.addEventListener("keydown", squatter)

    escape()

    expect(calls).toBe(1)
    document.removeEventListener("keydown", squatter)
    detach()
  })

  test("detach removes the document listener", () => {
    let calls = 0
    const detach = listenForEscape(() => calls++)

    detach()
    escape()

    expect(calls).toBe(0)
  })

  test("ignores other keys and IME composition", () => {
    let calls = 0
    const detach = listenForEscape(() => calls++)

    const other = new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true })
    document.dispatchEvent(other)
    escape({ key: "Escape" })
    escape({ isComposing: true })

    expect(calls).toBe(1)
    detach()
  })
})

describe("closeMobilePanel", () => {
  const opener = () => {
    const element = document.createElement("button")
    element.textContent = "open"
    document.body.appendChild(element)
    return element
  }

  test("closes first, then focuses the opener on the next microtask", async () => {
    const trigger = opener()
    const order: string[] = []
    let closed = false

    closeMobilePanel({
      onClose: () => {
        closed = true
        order.push("close")
      },
      returnFocus: () => {
        order.push("focus")
        return trigger
      },
    })

    // The close is synchronous; the focus handoff is not, because hosts unmount
    // the sheet in the same tick and a still-mounted sheet would steal it back.
    expect(closed).toBe(true)
    expect(order).toEqual(["close"])

    await Promise.resolve()

    expect(order).toEqual(["close", "focus"])
    expect(document.activeElement).toBe(trigger)
    trigger.remove()
  })

  test("still closes when the opener is gone", () => {
    const trigger = opener()
    trigger.remove()

    let closed = false
    expect(() =>
      closeMobilePanel({
        onClose: () => {
          closed = true
        },
        returnFocus: () => undefined,
      }),
    ).not.toThrow()
    expect(closed).toBe(true)
  })
})
