import { describe, expect, test } from "bun:test"
import { createEventRefetchDebounce } from "./agent-queries"

/** A manual clock so the test owns every timer, with no real 600 ms wait. */
function fakeTimers() {
  let now = 0
  let nextID = 1
  const scheduled = new Map<number, { at: number; run: () => void }>()

  return {
    timers: {
      setTimeout(run: () => void, delay: number) {
        const id = nextID++
        scheduled.set(id, { at: now + delay, run })

        return id as unknown as ReturnType<typeof setTimeout>
      },
      clearTimeout(id: ReturnType<typeof setTimeout>) {
        scheduled.delete(id as unknown as number)
      },
    },
    advance(ms: number) {
      const target = now + ms
      for (;;) {
        const due = [...scheduled.entries()]
          .filter(([, entry]) => entry.at <= target)
          .sort((a, b) => a[1].at - b[1].at)[0]
        if (!due) break
        scheduled.delete(due[0])
        now = due[1].at
        due[1].run()
      }
      now = target
    },
    pending: () => scheduled.size,
  }
}

describe("createEventRefetchDebounce", () => {
  test("collapses a burst of events into exactly one trailing refetch", () => {
    const clock = fakeTimers()
    let calls = 0
    const debounce = createEventRefetchDebounce(() => calls++, 600, clock.timers)

    for (let index = 0; index < 10; index++) {
      debounce.schedule()
      clock.advance(50)
    }
    expect(calls).toBe(0)
    expect(clock.pending()).toBe(1)

    clock.advance(600)
    expect(calls).toBe(1)
    expect(clock.pending()).toBe(0)
  })

  test("restarts the timer on each event instead of firing early", () => {
    const clock = fakeTimers()
    let calls = 0
    const debounce = createEventRefetchDebounce(() => calls++, 600, clock.timers)

    debounce.schedule()
    clock.advance(599)
    debounce.schedule()
    clock.advance(599)
    expect(calls).toBe(0)

    clock.advance(1)
    expect(calls).toBe(1)
  })

  test("cancel clears a pending timer and a later event still fires", () => {
    const clock = fakeTimers()
    let calls = 0
    const debounce = createEventRefetchDebounce(() => calls++, 600, clock.timers)

    debounce.schedule()
    debounce.cancel()
    expect(debounce.pending()).toBe(false)
    clock.advance(1_000)
    expect(calls).toBe(0)

    debounce.schedule()
    clock.advance(600)
    expect(calls).toBe(1)
  })

  test("a disposed panel leaves no timer behind", () => {
    const clock = fakeTimers()
    let calls = 0
    const debounce = createEventRefetchDebounce(() => calls++, 600, clock.timers)

    debounce.schedule()
    debounce.cancel()
    debounce.cancel()
    expect(clock.pending()).toBe(0)
    clock.advance(5_000)
    expect(calls).toBe(0)
  })
})
