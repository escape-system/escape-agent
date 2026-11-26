import { describe, expect, test } from "bun:test"
import { resolveSessionComposerSelection } from "./selection"

describe("resolveSessionComposerSelection", () => {
  test("prefers durable Session state over historical message metadata", () => {
    expect(
      resolveSessionComposerSelection(
        { agent: "build", model: { id: "claude", providerID: "anthropic" } },
        { agent: "review", model: { modelID: "gpt", providerID: "openai" } },
      ),
    ).toEqual({
      agent: "build",
      model: { modelID: "claude", providerID: "anthropic", variant: undefined },
    })
  })

  test("falls back to historical metadata while durable state is unavailable", () => {
    expect(
      resolveSessionComposerSelection(undefined, {
        agent: "review",
        model: { modelID: "gpt", providerID: "openai", variant: "high", credentialID: "cred_2" },
      }),
    ).toEqual({
      agent: "review",
      model: { modelID: "gpt", providerID: "openai", variant: "high", credentialID: "cred_2" },
    })
  })

  test("preserves credentialID from durable session state", () => {
    expect(
      resolveSessionComposerSelection(
        { agent: "build", model: { id: "glm", providerID: "opencode-go", credentialID: "cred_1" } },
        undefined,
      ),
    ).toEqual({
      agent: "build",
      model: { modelID: "glm", providerID: "opencode-go", variant: undefined, credentialID: "cred_1" },
    })
  })
})
