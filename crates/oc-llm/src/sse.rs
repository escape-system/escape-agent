//! Incremental Server-Sent Events parser.
//!
//! Handles chunk boundaries (including split UTF-8), CRLF and LF, comments,
//! multi-line `data:` fields, unknown fields, and the `OpenAI` `[DONE]` sentinel.
//! It never panics on malformed input.

/// One dispatched SSE event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Text lines are assembled per byte chunk; complete lines are decoded as UTF-8.
#[derive(Debug, Default)]
pub struct SseParser {
    line: Vec<u8>,
    data: String,
    event: Option<String>,
    retry: Option<u64>,
}

impl SseParser {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one byte chunk and return every event completed by it.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        let mut events = Vec::new();
        for byte in chunk {
            if *byte == b'\n' {
                let mut line = std::mem::take(&mut self.line);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                self.process_line(&line, &mut events);
            } else {
                self.line.push(*byte);
            }
        }
        events
    }

    /// Flush a trailing unterminated line and any pending event (query streams may
    /// end without a final blank line).
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if !self.line.is_empty() {
            let mut line = std::mem::take(&mut self.line);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.process_line(&line, &mut events);
        }
        self.dispatch(&mut events);
        events
    }

    /// Last `retry:` value seen, in milliseconds.
    #[must_use]
    pub fn retry(&self) -> Option<u64> {
        self.retry
    }

    fn process_line(&mut self, line: &[u8], events: &mut Vec<SseEvent>) {
        if line.is_empty() {
            self.dispatch(events);
            return;
        }
        if line.first() == Some(&b':') {
            return;
        }
        let line = String::from_utf8_lossy(line);
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            // A line without a colon is a field with an empty value. Unknown
            // fields (including malformed ones) are ignored.
            None => (line.as_ref(), ""),
        };
        match field {
            "data" => {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(value);
            }
            "event" => self.event = Some(value.to_string()),
            "retry" => self.retry = value.parse().ok(),
            _ => {}
        }
    }

    fn dispatch(&mut self, events: &mut Vec<SseEvent>) {
        if self.data.is_empty() && self.event.is_none() {
            self.data.clear();
            return;
        }
        events.push(SseEvent {
            event: self.event.take(),
            data: std::mem::take(&mut self.data),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiline_data_and_comments() {
        let mut parser = SseParser::new();
        let events = parser.push(
            b": keepalive\n\n\
              event: content_block_delta\n\
              data: first\n\
              data: second\n\n",
        );
        assert_eq!(
            events,
            vec![SseEvent {
                event: Some("content_block_delta".to_string()),
                data: "first\nsecond".to_string(),
            }]
        );
    }

    #[test]
    fn done_and_malformed_lines() {
        let mut parser = SseParser::new();
        let events = parser.push(
            b"data: {\"a\":1}\n\n\
              data: [DONE]\n\n\
              data:{\"b\":2}\n\n\
              this line is malformed\n\n",
        );
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: None,
                    data: "{\"a\":1}".to_string(),
                },
                SseEvent {
                    event: None,
                    data: "[DONE]".to_string(),
                },
                SseEvent {
                    event: None,
                    data: "{\"b\":2}".to_string(),
                },
            ]
        );
    }

    #[test]
    fn crlf_and_split_chunks() {
        let mut parser = SseParser::new();
        let mut events = parser.push(b"data: hel");
        events.extend(parser.push(b"lo\r\n\r\n"));
        assert_eq!(
            events,
            vec![SseEvent {
                event: None,
                data: "hello".to_string(),
            }]
        );
    }

    #[test]
    fn finish_dispatches_without_blank_line() {
        let mut parser = SseParser::new();
        assert!(parser.push(b"data: tail").is_empty());
        assert_eq!(
            parser.finish(),
            vec![SseEvent {
                event: None,
                data: "tail".to_string(),
            }]
        );
    }
}
