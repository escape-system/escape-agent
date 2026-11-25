import { createEffect, on, onCleanup, Show, type JSX, type ParentProps } from "solid-js"
import { createStore } from "solid-js/store"
import { Button } from "@opencode/ui/button"
import { DrawerContext } from "@opencode/plugin/gui/host"
import { useLanguage } from "@/runtime/i18n/language"
import { MobileDrawer, MobileDrawerClose, MobileDrawerContent, MobileDrawerLabel } from "./mobile-drawer"
import "./mobile-panel-drawer.css"

type DrawerView = { title: string; content: JSX.Element; trigger: HTMLElement }

/**
 * Attach the sheet's Escape handling to the document.
 *
 * This is deliberately not corvu's `closeOnEscapeKeyDown`: in the browser run the
 * dismissible layer never delivered Escape for this sheet, so the lifecycle owner
 * wires the listener itself. Two details are load-bearing:
 *
 * - it listens in the **capture** phase, because an app-level bubble handler that
 *   consumes Escape would otherwise keep the sheet open, and
 * - it **ignores `defaultPrevented`**, because handlers registered earlier on
 *   `document` (composers, command menus) call `preventDefault()` for their own
 *   purposes and that must not make a modal sheet undismissable.
 *
 * Returns the detach function, which the caller must run on cleanup so a closed
 * sheet leaves no document listener behind.
 */
export function listenForEscape(onEscape: () => void): () => void {
  const handler = (event: KeyboardEvent) => {
    if (event.key !== "Escape" || event.isComposing) return
    onEscape()
  }

  document.addEventListener("keydown", handler, { capture: true })

  return () => document.removeEventListener("keydown", handler, { capture: true })
}

/**
 * Close the sheet and hand focus back to its opener.
 *
 * The focus return happens on a microtask because hosts commonly unmount the
 * sheet in the same tick (`agent-drawer.tsx` renders it under `<Show>`), which
 * disposes the dialog before corvu's `finalFocusEl` restore can run. It is
 * idempotent: a host that also restores focus itself must not see a focus bounce.
 */
export function closeMobilePanel(options: {
  onClose: () => void
  returnFocus?: () => HTMLElement | undefined
}): void {
  options.onClose()
  queueMicrotask(() => {
    const target = options.returnFocus?.()
    if (target && document.activeElement !== target) target.focus()
  })
}

export function MobilePanelDrawer(
  props: ParentProps<{
    title: string
    hideHeader?: boolean
    open: boolean
    onOpenChange: (open: boolean) => void
    onContentPresentChange?: (present: boolean) => void
    initialFocus?: () => HTMLElement | undefined
    returnFocus?: () => HTMLElement | undefined
    onFinalFocus?: (event: Event) => void
  }>,
) {
  const language = useLanguage()
  const [store, setStore] = createStore<{ view?: DrawerView }>({})
  createEffect(
    on(
      () => props.open,
      (open) => {
        if (open) setStore("view", undefined)
      },
    ),
  )

  // The lifecycle owner wires Escape itself: corvu's dismissible layer does not
  // deliver Escape for this sheet in practice, and a host that unmounts the sheet
  // synchronously (the agent drawer's `<Show>`) disposes the dialog before
  // `finalFocusEl` can run. `closeMobilePanel` therefore closes and restores the
  // opener on a microtask, which is correct for both mounted and unmounting hosts.
  //
  // No `defer` here: hosts render this component inside `<Show when={open()}>`, so
  // it mounts with `open` already true and a deferred effect would never attach.
  createEffect(() => {
    if (!props.open) return

    const detach = listenForEscape(() => {
      closeMobilePanel({ onClose: () => props.onOpenChange(false), returnFocus: props.returnFocus })
    })

    onCleanup(detach)
  })

  const navigation = {
    close: () => props.onOpenChange(false),
    open: (view: DrawerView) => setStore("view", view),
    back: () => {
      const trigger = store.view?.trigger
      setStore("view", undefined)
      trigger?.focus()
    },
  }

  return (
    <DrawerContext.Provider value={navigation}>
      <MobileDrawer
        open={props.open}
        onOpenChange={props.onOpenChange}
        onContentPresentChange={props.onContentPresentChange}
        initialFocus={props.initialFocus}
        returnFocus={props.returnFocus}
        onFinalFocus={props.onFinalFocus}
        // Menu focus handoff must not dismiss the drawer during its opening transition.
        closeOnOutsideFocus={false}
      >
        <MobileDrawerContent>
          <div data-slot="mobile-panel" data-corvu-no-drag>
            <Show
              when={!props.hideHeader}
              fallback={<MobileDrawerLabel class="sr-only">{store.view?.title ?? props.title}</MobileDrawerLabel>}
            >
              <div data-slot="mobile-panel-header">
                <MobileDrawerLabel>{store.view?.title ?? props.title}</MobileDrawerLabel>
                <MobileDrawerClose
                  as={Button}
                  variant="ghost"
                  data-slot="mobile-panel-close"
                  aria-label={language.t("common.close")}
                >
                  {language.t("common.close")}
                </MobileDrawerClose>
              </div>
            </Show>
            <div data-slot="mobile-panel-content">
              <div hidden={!!store.view}>{props.children}</div>
              <Show when={store.view}>
                {(view) => (
                  <>
                    <Button
                      variant="ghost"
                      icon="arrow-left"
                      class="mb-2 !justify-start"
                      aria-label={language.t("common.goBack")}
                      onClick={navigation.back}
                    >
                      {props.title}
                    </Button>
                    {view().content}
                  </>
                )}
              </Show>
            </div>
          </div>
        </MobileDrawerContent>
      </MobileDrawer>
    </DrawerContext.Provider>
  )
}
