import { expect, test } from "bun:test"
import { syncPromptModel } from "./session-model-helpers"

const sonnet = { providerID: "anthropic", modelID: "claude-sonnet-4", variant: "high" }

test.each([
  ["stores the effective session model in prompt state", sonnet, undefined, [sonnet]],
  ["does not rewrite an unchanged prompt model", sonnet, sonnet, []],
  [
    "preserves and syncs credentialID when provided",
    { ...sonnet, credentialID: "cred_2" },
    sonnet,
    [{ ...sonnet, credentialID: "cred_2" }],
  ],
  [
    "replaces the submission mirror without changing the effective selection",
    { providerID: "openai", modelID: "gpt", variant: undefined },
    { providerID: "anthropic", modelID: "claude", variant: "high" },
    [{ providerID: "openai", modelID: "gpt", variant: undefined }],
  ],
])("%s", (_name, effective, mirror, writes) => {
  const calls: unknown[] = []
  const credentialID = "credentialID" in effective ? effective.credentialID : undefined

  syncPromptModel(
    {
      model: {
        current: () => ({ id: effective.modelID, provider: { id: effective.providerID } }),
        variant: { current: () => effective.variant },
        credential: { current: () => credentialID },
      },
    },
    { model: { current: () => mirror, set: (model) => calls.push(model) } },
  )

  expect(calls).toEqual(writes)
})
