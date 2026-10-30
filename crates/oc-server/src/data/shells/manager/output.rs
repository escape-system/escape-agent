//! Bounded, absolute-cursor byte window over combined shell output.

/// Bounded, absolute-cursor byte window over combined output.
pub(super) struct Output {
    data: Vec<u8>,
    head: usize,
    capacity: usize,
    start: u64,
    pub(super) size: u64,
}

impl Output {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            data: Vec::with_capacity(capacity.min(128 * 1024)),
            head: 0,
            capacity: capacity.max(1),
            start: 0,
            size: 0,
        }
    }

    pub(super) fn append(&mut self, chunk: &[u8]) {
        self.size = self
            .size
            .saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        self.data.extend_from_slice(chunk);
        let live = self.data.len() - self.head;
        if live <= self.capacity {
            return;
        }
        let mut cut = self.head + (live - self.capacity);
        // Do not start the retained window on a UTF-8 continuation byte.
        while cut < self.data.len() && self.data[cut] & 0xC0 == 0x80 {
            cut += 1;
        }
        self.start = self
            .start
            .saturating_add(u64::try_from(cut - self.head).unwrap_or(u64::MAX));
        self.head = cut;
        let compact_at = (self.capacity / 8).max(1);
        if self.head >= compact_at {
            self.data.drain(..self.head);
            self.head = 0;
        }
    }

    /// (text, cursor after this page, whether the requested cursor was
    /// evicted). A page never crosses the retained window's end.
    pub(super) fn read(&self, cursor: u64, limit: usize) -> (String, u64, bool) {
        let mut truncated = false;
        let mut start = cursor;
        if start < self.start {
            start = self.start;
            truncated = true;
        }
        if start > self.size {
            start = self.size;
        }
        let end = self
            .size
            .min(start.saturating_add(u64::try_from(limit).unwrap_or(u64::MAX)));
        let from = self.head + usize::try_from(start - self.start).unwrap_or(0);
        let to = self.head + usize::try_from(end - self.start).unwrap_or(0);
        let to = to.min(self.data.len());
        let from = from.min(to);
        (
            String::from_utf8_lossy(&self.data[from..to]).into_owned(),
            end,
            truncated,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::Output;

    #[test]
    fn pages_advance_the_absolute_cursor() {
        let mut output = Output::new(1024);
        output.append(b"abcdefghij");
        assert_eq!(output.read(0, 4), ("abcd".to_owned(), 4, false));
        assert_eq!(output.read(4, 4), ("efgh".to_owned(), 8, false));
        assert_eq!(output.read(8, 4), ("ij".to_owned(), 10, false));
        assert_eq!(output.read(10, 4), (String::new(), 10, false));
        assert_eq!(output.read(99, 4), (String::new(), 10, false));
        assert_eq!(output.read(0, 0), (String::new(), 0, false));
    }

    #[test]
    fn evicted_cursors_are_clamped_and_reported_truncated() {
        let mut output = Output::new(8);
        output.append(b"abcdefgh");
        output.append(b"ij");
        assert_eq!(output.size, 10);
        assert_eq!(output.start, 2);
        assert_eq!(output.read(0, 4), ("cdef".to_owned(), 6, true));
        assert_eq!(output.read(6, 4), ("ghij".to_owned(), 10, false));
        // Re-reading the oldest retained byte is no longer truncated.
        assert_eq!(output.read(2, 2), ("cd".to_owned(), 4, false));
    }

    #[test]
    fn utf8_continuation_bytes_are_not_split_at_the_front() {
        let mut output = Output::new(3);
        output.append("aé".as_bytes());
        output.append("z".as_bytes());
        // The window dropped `a`; it starts on the intact `é`.
        let (text, _, truncated) = output.read(0, 16);
        assert!(truncated);
        assert_eq!(text, "éz");
    }

    #[test]
    fn zero_limits_and_cursors_past_the_end_are_clamped() {
        let mut output = Output::new(1024);
        output.append(b"abcdefghij");
        assert_eq!(output.read(3, 0), (String::new(), 3, false));
        assert_eq!(output.read(999, 10), (String::new(), 10, false));
        assert_eq!(output.read(5, 100), ("fghij".to_owned(), 10, false));
        assert_eq!(output.read(u64::MAX, 10), (String::new(), 10, false));
    }

    #[test]
    fn eviction_never_starts_on_a_utf8_continuation_byte() {
        let mut output = Output::new(3);
        output.append("é".as_bytes());
        // The nominal eviction point (byte 1) is the continuation of `é`, so
        // the whole character is dropped instead of producing invalid UTF-8.
        output.append(b"yz");
        assert_eq!(output.size, 4);
        assert_eq!(output.start, 2);
        assert_ne!(output.data[output.head] & 0xC0, 0x80);
        assert_eq!(output.read(0, 16), ("yz".to_owned(), 4, true));
        assert_eq!(output.read(2, 16), ("yz".to_owned(), 4, false));

        // Further multibyte appends still leave the window on a boundary.
        output.append("ü!".as_bytes());
        let (text, cursor, _) = output.read(0, 16);
        assert_eq!(text, "ü!");
        assert_eq!(cursor, 7);
    }
}
