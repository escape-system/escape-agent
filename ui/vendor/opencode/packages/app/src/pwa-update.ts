// The service worker precaches the app shell and is built with `skipWaiting: false` and `clientsClaim: false`
// (`vite.pwa.ts`), on purpose: an open tab must never mix an old document with chunks from a new build. The cost is
// that a deployed build never reaches a browser that already registered the worker — not even on a plain reload,
// because the worker answers the navigation from its precache. Without a listener nothing would ever pick the new
// build up.
//
// So, once per new build, drop the stale shell completely: delete every cache, unregister the worker and reload into
// the new build. The reload loads a document that no stale worker controls, so it cannot loop; the session flag is a
// second guard for the case where the browser hands the page back to a worker again.

const RELOAD_FLAG = "opencode.pwa.reload-for"

/** Clears the precache and reloads once per build. Returns whether a reload was started. */
export async function takeOverWaitingBuild(worker: ServiceWorker) {
  const flag = `${RELOAD_FLAG}:${worker.scriptURL}`
  let previous: string | undefined

  try {
    previous = window.sessionStorage.getItem(flag) ?? undefined
  } catch {}

  // A build already took over in this tab; the next one waits for the user's next visit rather than looping.
  if (previous) return false

  try {
    window.sessionStorage.setItem(flag, Date.now().toString())
  } catch {}

  try {
    const keys = await caches.keys()
    await Promise.all(keys.map((key) => caches.delete(key)))
  } catch {}

  try {
    const registrations = await navigator.serviceWorker.getRegistrations()
    await Promise.all(registrations.map((entry) => entry.unregister()))
  } catch {}

  window.location.reload()

  return true
}

/**
 * Registers the service worker and switches to a newer deployed build as soon as one installs, instead of leaving the
 * tab on the shell it was opened with. Does nothing outside a production build or a browser without service workers.
 */
export function registerServiceWorker() {
  if (!import.meta.env.PROD || !("serviceWorker" in navigator)) return

  // The first install has no controller: that page already loaded this build, so there is nothing to replace.
  const activate = (worker: ServiceWorker | null | undefined) => {
    if (!worker || !navigator.serviceWorker.controller) return

    void takeOverWaitingBuild(worker)
  }

  void navigator.serviceWorker
    .register("/sw.js")
    .then((registration) => {
      // A worker that installed while this tab was closed is already waiting.
      activate(registration.waiting)

      registration.addEventListener("updatefound", () => {
        const installing = registration.installing

        if (!installing) return

        installing.addEventListener("statechange", () => {
          if (installing.state === "installed") activate(registration.waiting ?? installing)
        })
      })

      // Checks for a newly deployed build on every load and when the tab returns to the foreground.
      const update = () => void registration.update().catch(() => {})

      window.addEventListener("load", update, { once: true })
      document.addEventListener("visibilitychange", () => {
        if (document.visibilityState === "visible") update()
      })
    })
    .catch((error: unknown) => console.error("[pwa] service worker registration failed", error))
}
