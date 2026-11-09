import type { Accessor } from "solid-js"
import type { ActiveComposerAdapter, ComposerControls, ComposerSession } from "@/composer/adapter"
import { useComposerState } from "@/composer/persistence"
import { useData } from "@/runtime/server/current"
import { useServerSDK } from "@/runtime/server/client"
import { createSessionMessageHandoff } from "@/session/handoff"
import { useWorkspaceLocation } from "@/workspaces/location"

export function createActiveComposerAdapter(input: {
  sessionID: string
  sessionKey: Accessor<string>
  controls: Accessor<ComposerControls>
  submitted: () => void
  setEditor: (element: HTMLDivElement) => void
  background: NonNullable<ComposerSession["background"]>
}) {
  const id = input.sessionID

  const prompt = useComposerState()
  prompt.current()
  const state = prompt.capture()
  const data = useData()
  const server = useServerSDK()
  const location = useWorkspaceLocation()

  const adapter: ActiveComposerAdapter = {
    kind: "active-session",
    state,
    ready: prompt.ready,
    controls: input.controls,
    working: () => data.session.status(id) === "running",
    submitted: input.submitted,
    setEditor: input.setEditor,
    session: () => ({
      id,
      directory: location().directory,
      handoff: createSessionMessageHandoff(input.sessionKey(), id, server.event),
      api: server.api.session,
      data: {
        location: data.location,
        session: {
          prompt: data.session.prompt,
          setStatus: data.session.setStatus,
          requestStop: data.session.requestStop,
          releaseStop: data.session.releaseStop,
        },
      },
      current: () => data.session.get(id),
      admitted: (messageID) => data.session.input.has(id, messageID) || !!data.session.message.get(id, messageID),
      background: input.background,
    }),
    // Stop means stop: never resume the run. The rejection reaches the caller, which reports it and restores the
    // status it cleared optimistically.
    interrupt: () => server.api.session.interrupt({ sessionID: id, resume: false }).then(() => undefined),
  }

  return adapter
}
