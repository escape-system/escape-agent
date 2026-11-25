import { onCleanup, onMount } from "solid-js"

/**
 * Coverage (CSS px) below which a visual-viewport shrink is browser chrome or a
 * pinch gesture, not an on-screen keyboard. iOS toolbars move the viewport by a
 * few dozen pixels; the smallest phone keyboard is far taller.
 */
export const KEYBOARD_MIN_COVERAGE = 100

/** Whether a focus target takes text, which is the only state that raises a keyboard. */
export function isEditableTarget(target: Element | null): boolean {
  return (
    target instanceof HTMLElement &&
    (target.isContentEditable || target.matches("input:not([readonly]), textarea:not([readonly])"))
  )
}

/**
 * The height an on-screen keyboard covers, in CSS pixels. `layoutHeight` is the
 * layout viewport (or `window.innerHeight`), `height`/`offsetTop`/`scale` come
 * from `visualViewport`: the visible CSS height is `height * scale`, so a pinch
 * zoom — which scales the visual viewport with the layout — never reads as a
 * keyboard. A keyboard shrinks the visual viewport without resizing the layout
 * viewport on iOS; on Android `interactive-widget=resizes-content` already
 * shrinks both, which correctly yields 0 here.
 */
export function keyboardInset(input: {
  layoutHeight: number
  height: number
  offsetTop?: number
  scale?: number
}): number {
  // Unknown metrics never claim a keyboard: a transparent composer is better than a phantom gap.
  if (!Number.isFinite(input.height) || !Number.isFinite(input.layoutHeight)) return 0
  const scale = Number.isFinite(input.scale) && (input.scale ?? 0) > 0 ? input.scale! : 1
  const covered = input.layoutHeight - (input.offsetTop ?? 0) - input.height * scale
  if (!Number.isFinite(covered) || covered <= KEYBOARD_MIN_COVERAGE) return 0
  return Math.round(covered)
}

/** Test seam: `root.style` is the only DOM surface the component writes. */
type InsetTarget = { style: Pick<CSSStyleDeclaration, "setProperty" | "removeProperty"> }

export function KeyboardInsets(options: { target?: InsetTarget; viewport?: VisualViewport | null } = {}) {
  onMount(() => {
    const target = options.target ?? document.documentElement
    const viewport = options.viewport === undefined ? window.visualViewport : options.viewport
    const style = target.style
    const layoutHeight = () =>
      (target as unknown as { clientHeight?: number }).clientHeight || window.innerHeight

    const sync = () => {
      const inset = isEditableTarget(document.activeElement)
        ? keyboardInset({
            layoutHeight: layoutHeight(),
            height: viewport?.height ?? window.innerHeight,
            offsetTop: viewport?.offsetTop,
            scale: viewport?.scale,
          })
        : 0

      // Always publish a value: consumers read `var(--keyboard-inset)` without a fallback;
      // `index.css` seeds 0px for the pre-mount frame.
      style.setProperty("--keyboard-inset", `${inset}px`)

      // iOS retains the home-indicator inset above its keyboard.
      if (inset > 0) style.setProperty("--safe-area-inset-bottom", "0px")
      else style.removeProperty("--safe-area-inset-bottom")
    }

    sync()
    viewport?.addEventListener("resize", sync)
    viewport?.addEventListener("scroll", sync)
    window.addEventListener("resize", sync)
    document.addEventListener("focusin", sync)
    document.addEventListener("focusout", sync)
    onCleanup(() => {
      viewport?.removeEventListener("resize", sync)
      viewport?.removeEventListener("scroll", sync)
      window.removeEventListener("resize", sync)
      document.removeEventListener("focusin", sync)
      document.removeEventListener("focusout", sync)
      style.removeProperty("--keyboard-inset")
      style.removeProperty("--safe-area-inset-bottom")
    })
  })

  return null
}
