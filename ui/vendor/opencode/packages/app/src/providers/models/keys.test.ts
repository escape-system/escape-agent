import { describe, expect, test } from "bun:test"
import type { IntegrationInfo } from "@opencode/client/promise"
import {
  listProviderKeys,
  resolveIntegrationForProvider,
  resolveSelectedKeyLabel,
  resolveSubagentKeyLabel,
} from "./keys"

const goIntegration: IntegrationInfo = {
  id: "opencode-go",
  name: "OpenCode Go",
  methods: [],
  connections: [
    { type: "credential", id: "cred_b", label: "OpenCode Go", method: "key" },
    { type: "credential", id: "cred_a", label: "OpenCode Go", method: "key", active: true },
  ],
}

describe("provider keys helper", () => {
  test("puts active credential first and numbers duplicate/generic labels as OpenCode Go 1, OpenCode Go 2", () => {
    const keys = listProviderKeys({
      integration: goIntegration,
      providerID: "opencode-go",
      providerName: "OpenCode Go",
      keyHealth: [{ providerID: "opencode-go", credentialID: "cred_b", untilMs: 5_000, kind: "rate_limit", failures: 2 }],
      nowMs: 1_000,
    })

    expect(keys).toEqual([
      {
        id: "cred_a",
        credentialID: "cred_a",
        label: "OpenCode Go 1",
        method: "credential",
        active: true,
        cooldownUntilMs: undefined,
        cooldownKind: undefined,
      },
      {
        id: "cred_b",
        credentialID: "cred_b",
        label: "OpenCode Go 2",
        method: "credential",
        active: false,
        cooldownUntilMs: 5_000,
        cooldownKind: "rate_limit",
      },
    ])
  })

  test("preserves distinct custom credential labels and resolves chip & subagent labels", () => {
    const integration: IntegrationInfo = {
      id: "anthropic",
      name: "Anthropic",
      methods: [],
      connections: [
        { type: "credential", id: "cred_work", label: "Work", method: "key", active: true },
        { type: "credential", id: "cred_personal", label: "Personal", method: "key" },
      ],
    }

    const keys = listProviderKeys({ integration, providerID: "anthropic", providerName: "Anthropic" })
    expect(keys.map((k) => k.label)).toEqual(["Work", "Personal"])
    expect(resolveSelectedKeyLabel({ credentialID: "cred_personal", keys, autoLabel: "Auto" })).toBe("Personal")
    expect(resolveSelectedKeyLabel({ credentialID: undefined, keys, autoLabel: "Auto" })).toBe("Auto")
    expect(resolveSelectedKeyLabel({ credentialID: undefined, keys: [keys[0]], autoLabel: "Auto" })).toBe("Work")
    expect(resolveSubagentKeyLabel({ credentialID: "cred_personal", keys })).toBe("Personal")
    expect(resolveSubagentKeyLabel({ credentialID: undefined, keys })).toBeUndefined()
  })

  test("resolves integration by explicit integrationID before providerID fallback", () => {
    const consoleInt: IntegrationInfo = { id: "opencode", name: "OpenCode", methods: [], connections: [] }
    expect(
      resolveIntegrationForProvider([consoleInt, goIntegration], {
        providerID: "opencode-go",
        integrationID: "opencode",
      })?.id,
    ).toBe("opencode")
    expect(
      resolveIntegrationForProvider([consoleInt, goIntegration], {
        providerID: "opencode-go",
      })?.id,
    ).toBe("opencode-go")
  })
})
