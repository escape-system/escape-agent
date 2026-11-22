import { afterEach, beforeEach, describe, expect, test } from "bun:test"
import { takeOverWaitingBuild } from "./pwa-update"

type Fake = { keys: string[]; deleted: string[]; unregistered: number; reloads: number }

const original = {
  caches: Object.getOwnPropertyDescriptor(globalThis, "caches"),
  reload: Object.getOwnPropertyDescriptor(window.location, "reload"),
  serviceWorker: Object.getOwnPropertyDescriptor(navigator, "serviceWorker"),
  storage: new Map<string, string>(),
}

let fake: Fake

beforeEach(() => {
  fake = { keys: ["workbox-precache-v2", "other"], deleted: [], unregistered: 0, reloads: 0 }
  original.storage.clear()

  Object.defineProperty(globalThis, "caches", {
    configurable: true,
    value: {
      keys: async () => [...fake.keys],
      delete: async (key: string) => {
        fake.deleted.push(key)

        return true
      },
    },
  })

  Object.defineProperty(globalThis.navigator, "serviceWorker", {
    configurable: true,
    value: {
      getRegistrations: async () => [{ unregister: async () => (fake.unregistered += 1) }],
    },
  })

  Object.defineProperty(window.location, "reload", {
    configurable: true,
    value: () => (fake.reloads += 1),
  })

  Object.defineProperty(window, "sessionStorage", {
    configurable: true,
    value: {
      getItem: (key: string) => original.storage.get(key) ?? null,
      setItem: (key: string, value: string) => void original.storage.set(key, value),
    },
  })
})

afterEach(() => {
  for (const [target, key] of [
    [globalThis, "caches"],
    [globalThis.navigator, "serviceWorker"],
    [window.location, "reload"],
    [window, "sessionStorage"],
  ] as const) {
    const descriptor = key === "reload" ? original.reload : key === "caches" ? original.caches : descriptorOf(target, key)
    if (descriptor) Object.defineProperty(target, key, descriptor)
  }
})

function descriptorOf(target: object, key: string) {
  if (key === "serviceWorker") return original.serviceWorker
  if (key === "sessionStorage") return Object.getOwnPropertyDescriptor(window, "sessionStorage")
  return Object.getOwnPropertyDescriptor(target, key)
}

const worker = { scriptURL: "http://127.0.0.1:8788/sw.js" } as ServiceWorker

describe("takeOverWaitingBuild", () => {
  test("clears every cache, unregisters the worker and reloads into the new build", async () => {
    expect(await takeOverWaitingBuild(worker)).toBe(true)
    expect(fake.deleted.sort()).toEqual(["other", "workbox-precache-v2"])
    expect(fake.unregistered).toBe(1)
    expect(fake.reloads).toBe(1)
  })

  test("takes over once per build, so a reload cannot loop", async () => {
    expect(await takeOverWaitingBuild(worker)).toBe(true)

    fake.deleted = []
    expect(await takeOverWaitingBuild(worker)).toBe(false)
    expect(fake.deleted).toEqual([])
    expect(fake.reloads).toBe(1)
  })

  test("still reloads when cache or registration access throws", async () => {
    Object.defineProperty(globalThis, "caches", {
      configurable: true,
      value: {
        keys: async () => {
          throw new Error("denied")
        },
      },
    })

    expect(await takeOverWaitingBuild(worker)).toBe(true)
    expect(fake.reloads).toBe(1)
  })
})
