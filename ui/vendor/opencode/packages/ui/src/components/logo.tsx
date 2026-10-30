import { type ComponentProps } from "solid-js"

// Escape Agent marks. The raster illustration is the canonical mark; the
// wordmark is drawn as the same 5×7 block letters used by the new-session
// stage so the flat logo, the hero and the pin board share one silhouette.

export const Mark = (props: { class?: string }) => {
  return (
    <svg
      data-component="logo-mark"
      classList={{ [props.class ?? ""]: !!props.class }}
      viewBox="0 0 16 20"
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
      aria-hidden="true"
    >
      <image
        href="/brand/escape-agent-mark.png"
        x="0"
        y="0"
        width="16"
        height="20"
        preserveAspectRatio="xMidYMid meet"
      />
    </svg>
  )
}

export const Splash = (props: Pick<ComponentProps<"svg">, "ref" | "class">) => {
  return (
    <svg
      ref={props.ref}
      data-component="logo-splash"
      classList={{ [props.class ?? ""]: !!props.class }}
      viewBox="0 0 80 100"
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
      aria-hidden="true"
    >
      <image
        href="/brand/escape-agent-mark.png"
        x="0"
        y="10"
        width="80"
        height="80"
        preserveAspectRatio="xMidYMid meet"
      />
    </svg>
  )
}

const LETTERS = [
  "M30 3.5h25v5h-25ZM30 8.5h5v5h-5ZM30 13.5h5v5h-5ZM30 18.5h20v5h-20ZM30 23.5h5v5h-5ZM30 28.5h5v5h-5ZM30 33.5h25v5h-25Z",
  "M65 3.5h20v5h-20ZM60 8.5h5v5h-5ZM60 13.5h5v5h-5ZM65 18.5h15v5h-15ZM80 23.5h5v5h-5ZM80 28.5h5v5h-5ZM60 33.5h20v5h-20Z",
  "M95 3.5h20v5h-20ZM90 8.5h5v5h-5ZM90 13.5h5v5h-5ZM90 18.5h5v5h-5ZM90 23.5h5v5h-5ZM90 28.5h5v5h-5ZM95 33.5h20v5h-20Z",
  "M125 3.5h15v5h-15ZM120 8.5h5v5h-5ZM140 8.5h5v5h-5ZM120 13.5h5v5h-5ZM140 13.5h5v5h-5ZM120 18.5h25v5h-25ZM120 23.5h5v5h-5ZM140 23.5h5v5h-5ZM120 28.5h5v5h-5ZM140 28.5h5v5h-5ZM120 33.5h5v5h-5ZM140 33.5h5v5h-5Z",
  "M150 3.5h20v5h-20ZM150 8.5h5v5h-5ZM170 8.5h5v5h-5ZM150 13.5h5v5h-5ZM170 13.5h5v5h-5ZM150 18.5h20v5h-20ZM150 23.5h5v5h-5ZM150 28.5h5v5h-5ZM150 33.5h5v5h-5Z",
  "M180 3.5h25v5h-25ZM180 8.5h5v5h-5ZM180 13.5h5v5h-5ZM180 18.5h20v5h-20ZM180 23.5h5v5h-5ZM180 28.5h5v5h-5ZM180 33.5h25v5h-25Z",
] as const

export const Logo = (props: { class?: string }) => {
  return (
    <svg
      xmlns="http://www.w3.org/2000/svg"
      viewBox="0 0 234 42"
      fill="none"
      classList={{ [props.class ?? ""]: !!props.class }}
      aria-hidden="true"
    >
      <g>
        {LETTERS.map((d) => (
          <g>
            <path d={d} fill="var(--icon-weak-base)" opacity="0.16" transform="translate(1.1 1.1)" />
            <path d={d} fill="currentColor" />
          </g>
        ))}
      </g>
    </svg>
  )
}
