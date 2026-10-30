//! Wire positions are UTF-16 code units (LSP's default encoding).
//!
//! Every conversion between wire positions and Rust byte offsets lives here so
//! the rest of the crate can work exclusively with byte offsets.

/// A zero-based LSP position (line, UTF-16 code unit within the line).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Position {
    /// Zero-based line number.
    pub line: u32,
    /// Zero-based UTF-16 code unit offset within the line.
    pub character: u32,
}

impl Position {
    /// Construct a position.
    #[must_use]
    pub const fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }

    /// Serialize to the JSON shape LSP expects.
    #[must_use]
    pub fn to_json(self) -> serde_json::Value {
        serde_json::json!({ "line": self.line, "character": self.character })
    }
}

/// Number of UTF-16 code units in `text`.
#[must_use]
pub fn utf16_len(text: &str) -> u32 {
    let mut units = 0_u32;
    for ch in text.chars() {
        units = units.saturating_add(utf16_width(ch));
    }
    units
}

fn utf16_width(character: char) -> u32 {
    u32::try_from(character.len_utf16()).unwrap_or(u32::MAX)
}

/// Byte offset of the start of a zero-based `line`, or `None` when out of range.
#[must_use]
pub fn line_start(text: &str, line: u32) -> Option<usize> {
    if line == 0 {
        return Some(0);
    }
    let mut seen = 0_u32;
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            seen += 1;
            if seen == line {
                return Some(index + 1);
            }
        }
    }
    None
}

/// Byte offset of the end of a zero-based `line` (excluding the newline).
#[must_use]
pub fn line_end(text: &str, line: u32) -> Option<usize> {
    let start = line_start(text, line)?;
    Some(start + text[start..].find('\n').unwrap_or(text.len() - start))
}

/// Convert a wire position to a byte offset.
///
/// Positions beyond the end of a line clamp to the line end; positions beyond
/// the last line clamp to the end of the document. A character offset that
/// lands inside a surrogate pair clamps to the start of that scalar value.
#[must_use]
pub fn position_to_offset(text: &str, line: u32, character: u32) -> Option<usize> {
    let start = line_start(text, line)?;
    let end = start + text[start..].find('\n').unwrap_or(text.len() - start);
    let line_text = &text[start..end];

    let mut units = 0_u32;
    let mut bytes = 0_usize;
    for ch in line_text.chars() {
        if units >= character {
            break;
        }
        let width = utf16_width(ch);
        if units.saturating_add(width) > character {
            // The caller pointed inside a surrogate pair; use the boundary.
            break;
        }
        units += width;
        bytes += ch.len_utf8();
    }
    Some(start + bytes)
}

/// Convert a byte offset to a wire position, clamping to a character boundary
/// and to the document length.
#[must_use]
pub fn offset_to_position(text: &str, offset: usize) -> Position {
    let mut offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let prefix = &text[..offset];
    let line = prefix.bytes().filter(|&b| b == b'\n').count();
    let line_start = prefix.rfind('\n').map_or(0, |index| index + 1);
    Position::new(as_u32(line), utf16_len(&prefix[line_start..]))
}

/// Convert a one-based user line/character pair into a wire position clamped to
/// the document, returning the byte offset too.
#[must_use]
pub fn user_position_to_offset(text: &str, line: u32, character: u32) -> (Position, usize) {
    let line = line.saturating_sub(1);
    let character = character.saturating_sub(1);
    let offset = position_to_offset(text, line, character).unwrap_or(text.len());
    (offset_to_position(text, offset), offset)
}

fn as_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_positions_round_trip() {
        let text = "alpha\nbeta\ngamma";
        assert_eq!(position_to_offset(text, 0, 0), Some(0));
        assert_eq!(position_to_offset(text, 1, 2), Some(8));
        assert_eq!(position_to_offset(text, 2, 5), Some(16));
        assert_eq!(offset_to_position(text, 8), Position::new(1, 2));
    }

    #[test]
    fn non_bmp_characters_use_surrogate_pairs() {
        // U+1F600 is 4 UTF-8 bytes and 2 UTF-16 code units.
        let text = "a😀b\nnext";
        assert_eq!(utf16_len("a😀b"), 4);
        assert_eq!(position_to_offset(text, 0, 1), Some(1));
        // Inside the surrogate pair clamps before the emoji.
        assert_eq!(position_to_offset(text, 0, 2), Some(1));
        assert_eq!(position_to_offset(text, 0, 3), Some(5));
        assert_eq!(position_to_offset(text, 0, 4), Some(6));
        assert_eq!(offset_to_position(text, 5), Position::new(0, 3));
        assert_eq!(offset_to_position(text, 6), Position::new(0, 4));
        // Second line starts after the newline.
        assert_eq!(offset_to_position(text, 7), Position::new(1, 0));
    }

    #[test]
    fn combiners_are_single_units() {
        let text = "e\u{0301}x";
        assert_eq!(utf16_len(text), 3);
        assert_eq!(position_to_offset(text, 0, 2), Some(3));
    }

    #[test]
    fn out_of_range_positions_clamp() {
        let text = "one\ntwo";
        assert_eq!(position_to_offset(text, 0, 99), Some(3));
        assert_eq!(position_to_offset(text, 99, 0), None);
        assert_eq!(offset_to_position(text, 999), Position::new(1, 3));
    }

    #[test]
    fn one_based_input_is_converted() {
        let text = "one\ntwo\n";
        let (position, offset) = user_position_to_offset(text, 2, 2);
        assert_eq!(position, Position::new(1, 1));
        assert_eq!(offset, 5);
    }

    #[test]
    fn crlf_line_endings_do_not_leak_carriage_return() {
        let text = "ab\r\ncd";
        assert_eq!(offset_to_position(text, 4), Position::new(1, 0));
        assert_eq!(position_to_offset(text, 0, 2), Some(2));
    }
}
