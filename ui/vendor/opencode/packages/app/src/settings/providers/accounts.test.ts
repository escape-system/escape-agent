import { describe, expect, test } from "bun:test"
import type { IntegrationInfo } from "@opencode/client/promise"
import { accountRoles, activeProviderAccount, backupAccounts, providerAccounts, type ProviderAccount } from "./accounts"

const integration = (connections: IntegrationInfo["connections"]): IntegrationInfo => ({
  id: "openai",
  name: "OpenAI",
  methods: [],
  connections,
})

const key = (id: string, label: string): ProviderAccount => ({ type: "credential", id, label, method: "key" })

test("provider accounts keep the server's active-first credential order and ignore environment keys", () => {
  const value = integration([
    { type: "credential", id: "cred_work", label: "Work", method: "key" },
    { type: "env", name: "OPENAI_API_KEY" },
    { type: "credential", id: "cred_personal", label: "Personal", method: "oauth" },
  ])

  expect(providerAccounts(value)).toEqual([
    { type: "credential", id: "cred_work", label: "Work", method: "key" },
    { type: "credential", id: "cred_personal", label: "Personal", method: "oauth" },
  ])
  expect(activeProviderAccount(value)).toEqual({ type: "credential", id: "cred_work", label: "Work", method: "key" })

  const env = integration([{ type: "env", name: "OPENAI_API_KEY" }])
  expect(providerAccounts(env)).toEqual([])
  expect(activeProviderAccount(env)).toBeUndefined()
  expect(backupAccounts(env)).toEqual([])
  expect(providerAccounts(undefined)).toEqual([])
  expect(activeProviderAccount(undefined)).toBeUndefined()
})

describe("accountRoles", () => {
  test("without a marker the first (newest) account is primary", () => {
    const work = key("cred_work", "Work")
    const personal = key("cred_personal", "Personal")

    expect(accountRoles([work, personal])).toEqual({ primary: work, backups: [personal] })
    expect(accountRoles([work, personal], "cred_missing")).toEqual({ primary: work, backups: [personal] })
  })

  test("an explicit primary id wins over the wire order", () => {
    const work = key("cred_work", "Work")
    const personal = key("cred_personal", "Personal")

    expect(accountRoles([work, personal], "cred_personal")).toEqual({ primary: personal, backups: [work] })
  })

  test("a single account has no backups", () => {
    const work = key("cred_work", "Work")

    expect(accountRoles([work])).toEqual({ primary: work, backups: [] })
    expect(accountRoles([work], "cred_work")).toEqual({ primary: work, backups: [] })
  })

  test("an empty list has neither a primary nor backups", () => {
    const none: ProviderAccount[] = []

    expect(accountRoles(none).primary).toBeUndefined()
    expect(accountRoles(none).backups).toEqual([])
    expect(accountRoles(none, "cred_work").primary).toBeUndefined()
    expect(accountRoles(none, "cred_work").backups).toEqual([])
  })

  test("backups keep the server's order and never contain the primary twice", () => {
    const first = key("cred_first", "First")
    const second = key("cred_second", "Second")
    const third = key("cred_third", "Third")

    expect(accountRoles([first, second, third], "cred_second")).toEqual({
      primary: second,
      backups: [first, third],
    })
  })
})

test("backupAccounts drops the primary account and keeps the rest in order", () => {
  const value = integration([
    { type: "credential", id: "cred_work", label: "Work", method: "key" },
    { type: "env", name: "OPENAI_API_KEY" },
    { type: "credential", id: "cred_personal", label: "Personal", method: "key" },
  ])

  expect(backupAccounts(value)).toEqual([{ type: "credential", id: "cred_personal", label: "Personal", method: "key" }])
  expect(backupAccounts(integration([{ type: "credential", id: "cred_only", label: "Only", method: "key" }]))).toEqual([])
})
