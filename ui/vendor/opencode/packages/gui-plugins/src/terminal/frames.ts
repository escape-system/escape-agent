import { Option, Schema } from "effect"

/** One frame from the PTY socket, already classified. */
export type PtyFrame =
  /** Shell output to write to the terminal. `bytes` is its length in the stream (the cursor advances by it). */
  | { readonly type: "output"; readonly text: string; readonly bytes: number }
  /** A control frame carrying the server's byte cursor. */
  | { readonly type: "cursor"; readonly cursor: number | undefined }
  /** Nothing to do: an empty frame, or a control frame we cannot read. */
  | { readonly type: "ignore" }

// The server marks a control frame with a leading NUL, then JSON such as `{"cursor":391}`; every other binary frame is
// PTY output (see `oc-server/src/data/pty.rs`). Reading all binary frames as control frames dropped the entire shell
// output and left the terminal black.
const ControlFrame = Schema.fromJsonString(Schema.Struct({ cursor: Schema.optional(Schema.Number) }))

const decodeControlFrame = Schema.decodeUnknownOption(ControlFrame)

const controlFrame = (payload: string): PtyFrame => {
  const meta = decodeControlFrame(payload)

  if (Option.isNone(meta)) return { type: "ignore" }

  const cursor = meta.value.cursor

  return { type: "cursor", cursor: cursor !== undefined && Number.isSafeInteger(cursor) && cursor >= 0 ? cursor : undefined }
}

/**
 * Classifies one socket frame. Binary frames are PTY output unless they start with NUL; text frames are output unless
 * they too carry a NUL-prefixed control payload.
 */
export function classifyPtyFrame(data: unknown, decode: (bytes: Uint8Array) => string): PtyFrame {
  if (data instanceof ArrayBuffer) {
    const bytes = new Uint8Array(data)

    if (bytes.length === 0) return { type: "ignore" }

    if (bytes[0] === 0) return controlFrame(decode(bytes.subarray(1)))

    return { type: "output", text: decode(bytes), bytes: bytes.length }
  }

  if (ArrayBuffer.isView(data)) {
    const bytes = new Uint8Array(data.buffer, data.byteOffset, data.byteLength)

    return classifyPtyFrame(
      bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer,
      decode,
    )
  }

  const text = typeof data === "string" ? data : ""

  if (!text) return { type: "ignore" }
  const marker = text.indexOf("\u0000")

  if (marker !== -1) return controlFrame(text.slice(marker + 1))

  return { type: "output", text, bytes: text.length }
}
