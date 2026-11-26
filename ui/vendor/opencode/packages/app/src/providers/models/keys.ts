import type { IntegrationInfo } from "@opencode/client/promise"
import type { KeyHealth } from "@/settings/models/api"
import { accountRoles, primaryAccountID, providerAccounts } from "@/settings/providers/accounts"

export interface ProviderKeyOption {
  id: string
  credentialID?: string
  label: string
  method: "credential" | "env"
  active: boolean
  cooldownUntilMs?: number
  cooldownKind?: string
}

export function resolveIntegrationForProvider(
  integrations: readonly IntegrationInfo[],
  input: { providerID: string; integrationID?: string },
): IntegrationInfo | undefined {
  if (input.integrationID) {
    const byIntegration = integrations.find((item) => item.id === input.integrationID)

    if (byIntegration) return byIntegration
  }

  return integrations.find((item) => item.id === input.providerID)
}

export function disambiguateKeyLabels(
  labels: readonly string[],
  baseName: string,
  providerName?: string,
): string[] {
  const primaryBase = baseName.trim()
  const primaryLower = primaryBase.toLowerCase()
  const providerLower = providerName?.trim().toLowerCase()
  const counts = new Map<string, number>()

  for (const raw of labels) {
    const normalized = raw.trim().toLowerCase()

    counts.set(normalized, (counts.get(normalized) ?? 0) + 1)
  }

  return labels.map((raw, index) => {
    const trimmed = raw.trim()
    const lower = trimmed.toLowerCase()
    const isGeneric =
      !trimmed ||
      lower === primaryLower ||
      (providerLower !== undefined && lower === providerLower) ||
      (counts.get(lower) ?? 0) > 1

    return isGeneric ? `${trimmed || primaryBase} ${index + 1}` : trimmed
  })
}

export function listProviderKeys(input: {
  integration?: IntegrationInfo
  providerID: string
  providerName?: string
  keyHealth?: readonly KeyHealth[]
  nowMs?: number
}): ProviderKeyOption[] {
  const accounts = providerAccounts(input.integration)

  if (accounts.length === 0) return []

  const { primary, backups } = accountRoles(accounts, primaryAccountID(accounts))
  const ordered = primary ? [primary, ...backups] : [...accounts]
  const baseName = input.integration?.name?.trim() || input.providerName?.trim() || input.providerID
  const labels = disambiguateKeyLabels(
    ordered.map((account) => account.label),
    baseName,
    input.providerName,
  )

  const now = input.nowMs ?? Date.now()

  return ordered.map((account, index) => {
    const label = labels[index]
    const cooldown = input.keyHealth?.find(
      (entry) =>
        entry.providerID === input.providerID && entry.credentialID === account.id && entry.untilMs > now,
    )

    return {
      id: account.id,
      credentialID: account.id,
      label,
      method: "credential",
      active: primary?.id === account.id,
      cooldownUntilMs: cooldown?.untilMs,
      cooldownKind: cooldown?.kind,
    }
  })
}

export function resolveSelectedKeyLabel(input: {
  credentialID?: string
  keys: readonly ProviderKeyOption[]
  autoLabel: string
}): string | undefined {
  if (input.keys.length === 0) return undefined

  if (input.credentialID) {
    return input.keys.find((key) => key.credentialID === input.credentialID)?.label ?? input.credentialID
  }

  if (input.keys.length === 1) return input.keys[0].label

  return input.autoLabel
}

export function resolveSubagentKeyLabel(input: {
  credentialID?: string
  keys: readonly ProviderKeyOption[]
}): string | undefined {
  if (!input.credentialID) return undefined

  return input.keys.find((key) => key.credentialID === input.credentialID)?.label ?? input.credentialID
}
