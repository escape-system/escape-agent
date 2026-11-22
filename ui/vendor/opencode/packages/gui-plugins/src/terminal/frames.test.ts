import { describe, expect, test } from "bun:test"
import { classifyPtyFrame } from "./frames"

const decode = (bytes: Uint8Array) => new TextDecoder().decode(bytes)
const encode = (text: string) => {
  const bytes = new TextEncoder().encode(text)

  return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer
}

describe("classifyPtyFrame", () => {
  // Regression: every binary frame used to be read as a control frame, so a frame starting with ESC (shell output) was
  // dropped and the terminal stayed black while the shell ran.
  test("shell output arriving as a binary frame is output, not a control frame", () => {
    const frame = classifyPtyFrame(encode("\u001b]0;root@host: ~/project\u0007root@host:~# "), decode)

    expect(frame.type).toBe("output")
    expect(frame).toEqual({ type: "output", text: "\u001b]0;root@host: ~/project\u0007root@host:~# ", bytes: 38 })
  })

  test("output beginning with a carriage return is output", () => {
    const frame = classifyPtyFrame(encode("\r\u001b[K\r$ "), decode)

    expect(frame.type).toBe("output")
    expect(frame).toMatchObject({ text: "\r\u001b[K\r$ " })
  })

  test("a NUL-prefixed frame is the cursor control frame", () => {
    expect(classifyPtyFrame(encode('\u0000{"cursor":391}'), decode)).toEqual({ type: "cursor", cursor: 391 })
  })

  test("an unreadable control frame is ignored rather than treated as output", () => {
    expect(classifyPtyFrame(encode("\u0000not json"), decode)).toEqual({ type: "ignore" })
  })

  test("a cursor that is not a non-negative safe integer is not applied", () => {
    expect(classifyPtyFrame(encode('\u0000{"cursor":-1}'), decode)).toEqual({ type: "cursor", cursor: undefined })
    expect(classifyPtyFrame(encode('\u0000{"other":1}'), decode)).toEqual({ type: "cursor", cursor: undefined })
  })

  test("an empty frame is ignored", () => {
    expect(classifyPtyFrame(new ArrayBuffer(0), decode)).toEqual({ type: "ignore" })
  })

  test("text frames are output, and text frames carrying the control payload are cursor frames", () => {
    expect(classifyPtyFrame("hello", decode)).toEqual({ type: "output", text: "hello", bytes: 5 })
    expect(classifyPtyFrame('\u0000{"cursor":12}', decode)).toEqual({ type: "cursor", cursor: 12 })
  })

  test("a typed-array frame is classified like its buffer", () => {
    const bytes = new TextEncoder().encode("view-frame")
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength)

    expect(classifyPtyFrame(view, decode)).toEqual({ type: "output", text: "view-frame", bytes: 10 })
  })
})
