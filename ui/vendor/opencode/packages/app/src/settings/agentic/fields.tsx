import { Show, createEffect, createSignal, type Component } from "solid-js"
import { Switch } from "@opencode/ui/switch"
import { TextInput } from "@opencode/ui/text-input"
import { useLanguage } from "@/runtime/i18n/language"

/** A positive-integer config field; commits on blur or Enter and never sends an invalid value. */
export const NumberField: Component<{
  label: string
  value: number | undefined
  fallback: number
  testid?: string
  onCommit: (value: number) => void
}> = (props) => {
  const language = useLanguage()
  const [draft, setDraft] = createSignal("")
  const [invalid, setInvalid] = createSignal(false)
  let focused = false

  createEffect(() => {
    const value = props.value ?? props.fallback

    if (focused) return
    setDraft(String(value))
    setInvalid(false)
  })

  const commit = () => {
    const text = draft().trim()
    const current = props.value ?? props.fallback

    if (!/^\d+$/.test(text) || Number(text) < 1) {
      setInvalid(true)

      return
    }

    if (Number(text) === current) {
      setInvalid(false)

      return
    }

    setInvalid(false)
    props.onCommit(Number(text))
  }

  return (
    <div class="flex flex-col items-end gap-1" data-testid={props.testid} data-action={props.testid}>
      <TextInput
        type="number"
        inputmode="numeric"
        min="1"
        step="1"
        numeric
        class="w-24"
        value={draft()}
        invalid={invalid()}
        aria-label={props.label}
        onInput={(event) => setDraft(event.currentTarget.value)}
        onFocus={() => (focused = true)}
        onBlur={() => {
          focused = false
          commit()
        }}
        onKeyDown={(event) => {
          if (event.key === "Enter") event.currentTarget.blur()
        }}
      />
      <Show when={invalid()}>
        <span class="text-11-regular text-v2-state-fg-danger">{language.t("settings.agentic.invalidNumber")}</span>
      </Show>
    </div>
  )
}

/** A free-text config field; commits on blur or Enter and sends `null` for an emptied model reference. */
export const TextField: Component<{
  label: string
  value: string | undefined
  placeholder?: string
  testid?: string
  commit?: (value: string | null) => void
}> = (props) => {
  const [draft, setDraft] = createSignal("")
  let focused = false

  createEffect(() => {
    if (focused) return
    setDraft(props.value ?? "")
  })

  const commit = () => {
    if (!props.commit) return
    const current = props.value ?? ""
    const text = draft().trim()

    if (text === current) return

    props.commit(text ? text : null)
  }

  return (
    <TextInput
      type="text"
      class="w-56"
      value={draft()}
      placeholder={props.placeholder}
      aria-label={props.label}
      data-testid={props.testid}
      data-action={props.testid}
      spellcheck={false}
      onInput={(event) => setDraft(event.currentTarget.value)}
      onFocus={() => (focused = true)}
      onBlur={() => {
        focused = false
        commit()
      }}
      onKeyDown={(event) => {
        if (event.key === "Enter") event.currentTarget.blur()
      }}
    />
  )
}

export const ToggleField: Component<{
  label: string
  checked: boolean
  onChange: (checked: boolean) => void
  testid?: string
}> = (props) => {
  return (
    <div data-testid={props.testid} data-action={props.testid}>
      <Switch checked={props.checked} onChange={(checked) => props.onChange(checked)} hideLabel>
        {props.label}
      </Switch>
    </div>
  )
}

/** The restart banner the server asks for with `x-opencode-restart: required`. */
export const RestartNotice: Component<{ show: boolean }> = (props) => {
  const language = useLanguage()

  return (
    <Show when={props.show}>
      <div
        role="status"
        data-testid="settings-restart-notice"
        class="mb-4 flex flex-col gap-1 rounded-md border border-v2-state-border-warning bg-v2-state-bg-warning px-3 py-2"
      >
        <span class="text-12-medium text-v2-state-fg-warning">{language.t("settings.agentic.restart.title")}</span>
        <span class="text-11-regular text-v2-text-text-muted">{language.t("settings.agentic.restart.description")}</span>
      </div>
    </Show>
  )
}

/** A page-level failure: the server's message, never a toast. */
export const InlineError: Component<{ message: string | undefined; testid?: string }> = (props) => {
  return (
    <Show when={props.message}>
      {(message) => (
        <div
          role="alert"
          data-testid={props.testid ?? "settings-agentic-error"}
          class="mb-4 rounded-md border border-v2-state-border-danger bg-v2-state-bg-danger px-3 py-2 text-12-regular text-v2-state-fg-danger"
        >
          {message()}
        </div>
      )}
    </Show>
  )
}
