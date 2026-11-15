import { createEffect, on, onMount, type JSX } from "solid-js"
import { createStore } from "solid-js/store"
import { makeEventListener } from "@solid-primitives/event-listener"
import { createResizeObserver } from "@solid-primitives/resize-observer"
import { Icon } from "@opencode/ui/icon"
import { TextInput } from "@opencode/ui/text-input"
import "@/settings/search.css"

/**
 * Elements that own their keys. The typed-to-search shortcut only exists for a
 * page that has no input of its own in reach, so it must never take a key from
 * anything focusable, anything that announces itself as a control, or any
 * region that opted out with `data-settings-key-capture="off"` (the Models tab,
 * whose two search surfaces each handle their own keys).
 */
const INTERACTIVE =
  "input, textarea, select, [contenteditable], [contenteditable='true'], [role='combobox'], [role='listbox'], [role='option'], [role='button'], [role='menuitem'], [role='tab'], [role='switch'], [role='slider'], [role='progressbar'], button, a, summary, [data-settings-key-capture='off']"

const ownsKeys = (target: EventTarget | null) => target instanceof Element && !!target.closest(INTERACTIVE)

export function SettingsSearchField(props: {
  value: string
  placeholder: string
  /** The field's accessible name, when the placeholder names what it searches instead. */
  label?: string
  active: boolean
  onInput: (value: string) => void
  /** A live line under the field, such as the result count. */
  status?: JSX.Element
}) {
  const [state, setState] = createStore({ overflow: { start: false, end: false } })
  let root: HTMLDivElement | undefined
  let input: HTMLInputElement | undefined

  const updateOverflow = () => {
    if (!input) return
    const offset = Math.abs(input.scrollLeft)
    setState("overflow", {
      start: offset > 1,
      end: input.scrollWidth - input.clientWidth - offset > 1,
    })
  }

  const clear = () => {
    props.onInput("")
    input?.focus({ preventScroll: true })
  }

  createEffect(on(() => props.value, updateOverflow))
  onMount(() => {
    const screen = root?.closest<HTMLElement>(".settings-screen")

    if (!screen) return
    makeEventListener(screen, "keydown", (event) => {
      if (
        !props.active ||
        event.defaultPrevented ||
        event.isComposing ||
        event.ctrlKey ||
        event.metaKey ||
        event.altKey ||
        event.key.length !== 1 ||
        !event.key.trim() ||
        ownsKeys(event.target)
      )
        return
      event.preventDefault()
      event.stopPropagation()
      const value = `${props.value}${event.key}`
      props.onInput(value)
      input?.focus({ preventScroll: true })
      queueMicrotask(() => input?.setSelectionRange(value.length, value.length))
    })
  })

  return (
    <div ref={root} class="settings-tab-search settings-filter-search" data-component="settings-filter-search">
      <TextInput
        ref={(element) => {
          input = element
          createResizeObserver(element, updateOverflow)
        }}
        type="search"
        appearance="base"
        leadingIcon={<Icon name="magnifying-glass" size="small" />}
        value={props.value}
        data-overflow-start={state.overflow.start}
        data-overflow-end={state.overflow.end}
        onScroll={updateOverflow}
        onInput={(event) => props.onInput(event.currentTarget.value)}
        onKeyDown={(event) => {
          if (event.key !== "Escape" || event.defaultPrevented || event.isComposing) return
          event.preventDefault()
          event.stopPropagation()

          if (props.value) {
            clear()

            return
          }

          event.currentTarget.blur()
          event.currentTarget.closest<HTMLElement>(".settings-screen")?.focus({ preventScroll: true })
        }}
        placeholder={props.placeholder}
        aria-label={props.placeholder}
        showClearButton={!!props.value}
        clearIcon="circle-xmark"
        onClearClick={clear}
        spellcheck={false}
        autocorrect="off"
        autocomplete="off"
        autocapitalize="off"
      />
    </div>
  )
}
