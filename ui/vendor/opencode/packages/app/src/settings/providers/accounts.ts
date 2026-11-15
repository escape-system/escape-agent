import type { ConnectionInfo, IntegrationInfo } from "@opencode/client/promise"

export type ProviderAccount = Extract<ConnectionInfo, { type: "credential" }>

export function providerAccounts(integration: IntegrationInfo | undefined) {
  return integration?.connections.filter((connection): connection is ProviderAccount => connection.type === "credential") ?? []
}

// The wire carries no primary flag yet: `connections[]` arrives newest-first and the server moves the
// credential it activated to the front. A marker is still read defensively, so a future `active` field
// would be honoured without changing this client.
const markedPrimary = (account: ProviderAccount) => "active" in account && account.active === true

export function primaryAccountID(accounts: readonly ProviderAccount[]) {
  return accounts.find(markedPrimary)?.id
}

// The whole ordering decision, in one place and independent of Solid: an explicit primary wins, an
// unknown primary id falls back to the server's newest-first row, and nothing else can be primary.
export function accountRoles(accounts: readonly ProviderAccount[], primaryID?: string) {
  const explicit = primaryID === undefined ? undefined : accounts.find((account) => account.id === primaryID)
  const primary: ProviderAccount | undefined = explicit ?? accounts[0]

  return { primary, backups: accounts.filter((account) => account.id !== primary?.id) }
}

function integrationRoles(integration: IntegrationInfo | undefined) {
  const accounts = providerAccounts(integration)

  return accountRoles(accounts, primaryAccountID(accounts))
}

export function activeProviderAccount(integration: IntegrationInfo | undefined) {
  return integrationRoles(integration).primary
}

export function backupAccounts(integration: IntegrationInfo | undefined) {
  return integrationRoles(integration).backups
}
