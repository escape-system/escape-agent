import { describe, expect, mock, test } from "bun:test"

// `bun test --conditions=solid` — the documented unit command for this package — resolves `solid-js`
// to `dist/server.js`, the SSR build, where `createSignal` is a plain getter/setter pair with no
// observers and `createMemo` evaluates once without subscribing: a signal write never re-runs the
// memo, so the reactivity test below could only ever observe the empty mount list. Load the real
// client build and install it as `solid-js` for this file's
// registry (we run under `--isolate`, so no other test file is affected), then import the panel
// through that registry so its own memos get the same build. The specifier lives in a variable so
// `tsc` does not need declarations for the deep path.
const clientSolidSpecifier = "solid-js/dist/solid.js"
const clientSolid = await import(clientSolidSpecifier)
mock.module("solid-js", () => clientSolid)

const { createMemo, createRoot, createSignal } = await import("solid-js")
const { queuePanelRowIDs } = await import("./queue-panel")

// Regression for the browser symptom "the queue panel is visible but renders no row": the panel seeds
// its own `shown` list once at mount and mirrors later queue changes through a deferred transition
// effect (`{ defer: true }` runs only on change, never for the mount value). A panel that mounts while
// `pending.hydrate` is still in flight therefore has `live()` rows and an empty `shown` list; a strip
// that renders only `shown` stays visually empty. The render list must be the union: `shown` order for
// rows that are closing, plus every live row in queue order.
const row = (id: string) => ({ id })

describe("queuePanelRowIDs", () => {
  test("renders a queued user row that arrives after an empty mount", () => {
    // Mount value: shown = [] because the durable read had not returned yet.
    expect(queuePanelRowIDs([], [])).toEqual([])
    expect(queuePanelRowIDs([], ["msg_hydrated"])).toEqual(["msg_hydrated"])
  })

  test("renders a synthetic peer row that arrives after an empty mount", () => {
    expect(queuePanelRowIDs([], ["msg_peer"])).toEqual(["msg_peer"])
  })

  test("keeps queue order and leaves no gaps when every live row is already shown", () => {
    expect(queuePanelRowIDs([row("msg_a"), row("msg_b")], ["msg_a", "msg_b"])).toEqual(["msg_a", "msg_b"])
  })

  test("never duplicates a live row that is already shown", () => {
    expect(queuePanelRowIDs([row("msg_a")], ["msg_a", "msg_b"])).toEqual(["msg_a", "msg_b"])
  })

  test("keeps a closing row in its old slot until it is gone", () => {
    // `shown` holds the row being closed; `live` already dropped it.
    expect(queuePanelRowIDs([row("msg_gone"), row("msg_a")], ["msg_a"])).toEqual(["msg_gone", "msg_a"])
  })
})

// The same sequence the browser runs, with real Solid memos: mount with an empty durable queue, then
// hydrate rows into the signal. A render list derived from `live()` must re-evaluate; this is the
// assertion that would be RED if the strip read a mount-seeded snapshot (or if the post-mount update
// never reached the memo).
describe("queue render list under real Solid reactivity", () => {
  test("re-derives after the queue hydrates post-mount", () => {
    createRoot((dispose) => {
      const [liveRows, setLiveRows] = createSignal<string[]>([])
      const [shown, setShown] = createSignal<{ id: string }[]>([])
      const rendered = createMemo(() => queuePanelRowIDs(shown(), liveRows()))
      const seen: string[][] = []

      createMemo(() => {
        seen.push(rendered())
      })

      expect(seen.at(-1)).toEqual([])

      setLiveRows(["msg_hydrated", "msg_peer"])
      expect(seen.at(-1)).toEqual(["msg_hydrated", "msg_peer"])
      expect(rendered()).toEqual(["msg_hydrated", "msg_peer"])

      // A later in-place edit keeps the row visible.
      setShown([row("msg_hydrated"), row("msg_peer")])
      expect(seen.at(-1)).toEqual(["msg_hydrated", "msg_peer"])
      expect(rendered()).toEqual(["msg_hydrated", "msg_peer"])

      dispose()
    })
  })
})
