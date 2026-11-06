//! Bounded scrollback and the small ANSI state machine used by snapshots.

use bytes::Bytes;

/// Byte ring with absolute cursors and row indexes.
///
/// `data[head..]` is the live window. `start_byte` is the absolute offset of
/// its first byte and `end_byte` the total number of bytes ever appended.
/// `base_row` is the absolute index of the row containing `start_byte` (rows
/// increment on `\n`); after eviction that row can be a partial line.
pub(crate) struct RingBuffer {
    data: Vec<u8>,
    head: usize,
    capacity: usize,
    start_byte: u64,
    end_byte: u64,
    base_row: u64,
}

impl RingBuffer {
    pub(crate) fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        // Resident bound: the live window plus the dead prefix between
        // compactions (at most 1/8 buffer) and the reader's 8 KiB chunk.
        let reserve = capacity
            .saturating_add(capacity / 8)
            .saturating_add(2 * 8192);
        Self {
            data: Vec::with_capacity(reserve),
            head: 0,
            capacity,
            start_byte: 0,
            end_byte: 0,
            base_row: 0,
        }
    }

    pub(crate) fn append(&mut self, chunk: &[u8]) {
        self.data.extend_from_slice(chunk);
        self.end_byte = self
            .end_byte
            .saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        self.trim();
    }

    pub(crate) fn end_byte(&self) -> u64 {
        self.end_byte
    }

    pub(crate) fn start_byte(&self) -> u64 {
        self.start_byte
    }

    pub(crate) fn base_row(&self) -> u64 {
        self.base_row
    }

    fn retained(&self) -> &[u8] {
        &self.data[self.head..]
    }

    fn trim(&mut self) {
        let live = self.data.len() - self.head;
        if live <= self.capacity {
            return;
        }
        let mut cut = self.head + (live - self.capacity);
        // Do not leave a UTF-8 continuation byte at the front: snapshots must
        // not start with a replacement character for an intact character.
        while cut < self.data.len() && self.data[cut] & 0xC0 == 0x80 {
            cut += 1;
        }
        let dropped = cut - self.head;
        let mut newlines = 0_u64;
        for byte in &self.data[self.head..cut] {
            if *byte == b'\n' {
                newlines += 1;
            }
        }
        self.head = cut;
        self.start_byte = self
            .start_byte
            .saturating_add(u64::try_from(dropped).unwrap_or(u64::MAX));
        self.base_row = self.base_row.saturating_add(newlines);
        // Compact once the dead prefix reaches an eighth of the buffer. This
        // keeps resident memory within ~1.125 buffers while amortising the
        // memmove over at least `capacity / 8` bytes of output.
        let compact_at = (self.capacity / 8).max(1);
        if self.head >= compact_at {
            self.data.drain(..self.head);
            self.head = 0;
        }
    }

    /// Rows `[start, start + count)` by absolute index.
    ///
    /// The caller checks `start >= base_row` first. `count == 0` yields no
    /// rows. A trailing `\n` creates a final empty row, exactly as it does on
    /// a terminal.
    pub(crate) fn rows(&self, start: u64, count: usize) -> Vec<String> {
        let data = self.retained();
        let mut out = Vec::new();
        if count == 0 {
            return out;
        }
        let mut row = self.base_row;
        let mut line_start = 0;
        for (index, byte) in data.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            if row >= start {
                out.push(row_string(&data[line_start..index]));
                if out.len() == count {
                    return out;
                }
            }
            row += 1;
            line_start = index + 1;
        }
        if row >= start {
            out.push(row_string(&data[line_start..]));
        }
        out
    }

    /// Last `rows` logical lines, ANSI stripped, `\r` normalized, no trailing
    /// newline.
    pub(crate) fn snapshot(&self, rows: usize) -> String {
        if rows == 0 {
            return String::new();
        }
        let text = strip_ansi(self.retained());
        let mut lines: Vec<&str> = text.split('\n').collect();
        if lines.last() == Some(&"") {
            lines.pop();
        }
        let start = lines.len().saturating_sub(rows);
        lines[start..].join("\n")
    }

    /// Bytes from `cursor` (clamped into the retained window) to the end,
    /// plus the absolute end cursor.
    pub(crate) fn replay(&self, cursor: Option<u64>) -> (Bytes, u64) {
        let live = self.retained();
        let start = cursor.map_or(self.start_byte, |value| value.max(self.start_byte));
        let offset = usize::try_from(start - self.start_byte)
            .unwrap_or(usize::MAX)
            .min(live.len());
        (Bytes::copy_from_slice(&live[offset..]), self.end_byte)
    }
}

fn row_string(bytes: &[u8]) -> String {
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StripState {
    Ground,
    Esc,
    Csi,
    Osc,
    OscEsc,
    Dcs,
    Charset,
}

/// Remove ANSI CSI/OSC/DCS/ESC sequences and normalize `\r`.
///
/// `\r\n` becomes `\n`; a lone `\r` (carriage-return redraw) becomes `\n`.
/// Unterminated sequences are dropped. Invalid UTF-8 is replaced.
pub(crate) fn strip_ansi(input: &[u8]) -> String {
    let mut out = Vec::with_capacity(input.len());
    let mut state = StripState::Ground;
    let mut index = 0;
    while index < input.len() {
        let byte = input[index];
        match state {
            StripState::Ground => {
                if byte == 0x1B {
                    state = StripState::Esc;
                } else if byte == b'\r' {
                    while input.get(index + 1) == Some(&b'\r') {
                        index += 1;
                    }
                    if input.get(index + 1) != Some(&b'\n') {
                        out.push(b'\n');
                    }
                } else {
                    out.push(byte);
                }
            }
            StripState::Esc => match byte {
                b'[' => state = StripState::Csi,
                b']' => state = StripState::Osc,
                b'P' => state = StripState::Dcs,
                b'(' | b')' | b'*' | b'+' | b'-' | b'/' | b'#' | b'%' => {
                    state = StripState::Charset;
                }
                0x1B => {}
                _ => state = StripState::Ground,
            },
            StripState::Csi => {
                if (0x40..=0x7E).contains(&byte) {
                    state = StripState::Ground;
                } else if byte == 0x1B {
                    state = StripState::Esc;
                }
            }
            StripState::Osc | StripState::Dcs => {
                if byte == 0x07 {
                    state = StripState::Ground;
                } else if byte == 0x1B {
                    state = StripState::OscEsc;
                }
            }
            StripState::OscEsc => {
                state = if byte == b'\\' {
                    StripState::Ground
                } else {
                    StripState::Osc
                };
            }
            StripState::Charset => state = StripState::Ground,
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{RingBuffer, strip_ansi};

    #[test]
    fn strips_csi_and_osc() {
        assert_eq!(
            strip_ansi(b"\x1b[1;31mred\x1b[0m\r\nplain\n"),
            "red\nplain\n"
        );
        assert_eq!(strip_ansi(b"\x1b]0;title\x07body"), "body");
        assert_eq!(strip_ansi(b"\x1b]2;title\x1b\\body"), "body");
        assert_eq!(strip_ansi(b"\x1b(Babc"), "abc");
    }

    #[test]
    fn normalizes_carriage_returns() {
        assert_eq!(strip_ansi(b"a\r\nb"), "a\nb");
        assert_eq!(
            strip_ansi(b"progress 1\rprogress 2"),
            "progress 1\nprogress 2"
        );
    }

    #[test]
    fn rows_are_absolute_and_evict_from_the_front() {
        let mut buffer = RingBuffer::new(8);
        buffer.append(b"a\nb\nc\n");
        assert_eq!(buffer.base_row(), 0);
        assert_eq!(buffer.rows(0, 10), vec!["a", "b", "c", ""]);
        assert_eq!(buffer.end_byte(), 6);

        buffer.append(b"0123456789");
        assert!(buffer.end_byte() > 6);
        assert!(buffer.base_row() > 0);
        let oldest = buffer.base_row();
        let rows = buffer.rows(oldest, 2);
        assert!(!rows.is_empty());
    }

    #[test]
    fn replay_clamps_to_retained_window() {
        let mut buffer = RingBuffer::new(4);
        buffer.append(b"abcdef");
        let (bytes, cursor) = buffer.replay(Some(0));
        assert_eq!(bytes, Bytes::from_static(b"cdef"));
        assert_eq!(cursor, 6);
        let (bytes, cursor) = buffer.replay(Some(6));
        assert!(bytes.is_empty());
        assert_eq!(cursor, 6);
    }

    use bytes::Bytes;
}
