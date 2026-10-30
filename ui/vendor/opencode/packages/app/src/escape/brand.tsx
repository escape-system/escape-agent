import { Show } from "solid-js"

export const BRAND = {
  name: "Escape Agent",
  shortName: "Escape",
  wordmark: ["ESCAPE", "AGENT"],
  tagline: "Coding agent",
  repository: "https://github.com/opencode-rust/opencode-rust",
  mark: "/brand/escape-agent-mark.png",
  markLarge: "/brand/escape-agent.png",
  markSmall: "/brand/escape-agent-mark-128.png",
} as const

export function EscapeMark(props: {
  class?: string
  alt?: string
  src?: string
  decorative?: boolean
}) {
  return (
    <img
      src={props.src ?? BRAND.markSmall}
      alt={props.decorative === false ? (props.alt ?? BRAND.name) : ""}
      aria-hidden={props.decorative === false ? undefined : "true"}
      class={`ea-mark ${props.class ?? ""}`}
      draggable={false}
      decoding="async"
    />
  )
}

/** The full lockup used on the connect screen and in About. */
export function EscapeWordmark(props: { class?: string; tagline?: boolean; markClass?: string }) {
  return (
    <div class={`ea-connect-lockup ${props.class ?? ""}`} role="img" aria-label={BRAND.name}>
      <EscapeMark class={props.markClass} src={BRAND.mark} />
      <div class="ea-connect-word" aria-hidden="true">
        <span class="escape">{BRAND.wordmark[0]}</span>
        <span class="agent">{BRAND.wordmark[1]}</span>
      </div>
      <Show when={props.tagline !== false}>
        <div class="ea-connect-tagline" aria-hidden="true">
          {BRAND.tagline}
        </div>
      </Show>
    </div>
  )
}

/** The small lockup under the interactive new-session wordmark. */
export function EscapeBrandline() {
  return (
    <div class="ea-wordmark-brandline" aria-label={BRAND.name} role="img">
      <span aria-hidden="true">{BRAND.wordmark[1]}</span>
    </div>
  )
}
