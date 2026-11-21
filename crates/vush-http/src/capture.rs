//! The body of one response, as it is read: every byte is counted, and only
//! the first `limit` are kept.

/// A response body being read under a size limit.
pub(crate) struct Capture {
    limit: u64,
    kept: Vec<u8>,
    received: u64,
    truncated: bool,
}

impl Capture {
    /// An empty capture that keeps at most `limit` bytes.
    pub(crate) fn new(limit: u64) -> Self {
        Self {
            limit,
            kept: Vec::new(),
            received: 0,
            truncated: false,
        }
    }

    /// Counts `data` as received. It keeps bytes until `limit` are held, and
    /// marks the body truncated when it drops one.
    pub(crate) fn push(&mut self, data: &[u8]) {
        self.received = self.received.saturating_add(data.len() as u64);
        let held = self.kept.len() as u64;
        let room = usize::try_from(self.limit.saturating_sub(held)).unwrap_or(usize::MAX);
        if data.len() > room {
            self.truncated = true;
        }
        let take = data.len().min(room);
        self.kept.extend_from_slice(&data[..take]);
    }

    /// Every byte pushed so far, kept or not.
    pub(crate) fn received(&self) -> u64 {
        self.received
    }

    /// Takes the kept body and whether bytes were dropped. `received` stays
    /// readable afterwards.
    pub(crate) fn finish(&mut self) -> (Vec<u8>, bool) {
        (std::mem::take(&mut self.kept), self.truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_the_limit_is_kept_whole() {
        let mut capture = Capture::new(10);
        capture.push(b"abc");
        assert_eq!(capture.received(), 3);
        assert_eq!(capture.finish(), (b"abc".to_vec(), false));
    }

    #[test]
    fn exactly_at_the_limit_is_not_truncated() {
        let mut capture = Capture::new(3);
        capture.push(b"abc");
        assert_eq!(capture.finish(), (b"abc".to_vec(), false));
    }

    #[test]
    fn past_the_limit_is_cut_and_counted() {
        let mut capture = Capture::new(3);
        capture.push(b"abcd");
        capture.push(b"ef");
        assert_eq!(capture.received(), 6);
        assert_eq!(capture.finish(), (b"abc".to_vec(), true));
    }

    #[test]
    fn frames_crossing_the_limit() {
        let mut capture = Capture::new(5);
        capture.push(b"abc");
        capture.push(b"def");
        capture.push(b"g");
        assert_eq!(capture.received(), 7);
        assert_eq!(capture.finish(), (b"abcde".to_vec(), true));
    }

    #[test]
    fn a_zero_limit_keeps_nothing() {
        let mut capture = Capture::new(0);
        capture.push(b"a");
        assert_eq!(capture.received(), 1);
        assert_eq!(capture.finish(), (Vec::new(), true));
        let mut empty = Capture::new(0);
        empty.push(b"");
        assert_eq!(empty.finish(), (Vec::new(), false));
    }
}
