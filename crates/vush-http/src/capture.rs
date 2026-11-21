//! The body of one response, as it is read: every byte is counted, and at
//! most `limit` are kept. A body with a decoded encoding is decoded as it
//! arrives, and then the limit counts decoded bytes.

use vush_core::protocol::{Failure, FailureKind, FailureStage};

use crate::decode::{Corrupt, Decoder, Encoding};
use crate::text;

/// A response body being read under a size limit.
pub(crate) struct Capture {
    limit: u64,
    /// The body kept so far, decoded once decoding is done.
    kept: Vec<u8>,
    received: u64,
    truncated: bool,
    /// The decoder of a body that is decoded. Once its output passes the
    /// limit it is dropped, and `kept` holds that output.
    decoder: Option<Decoder>,
}

/// A body whose compressed stream is broken.
fn corrupt(_: Corrupt) -> Failure {
    Failure {
        kind: FailureKind::Protocol,
        stage: FailureStage::Body,
        message: text::BODY_CORRUPT,
    }
}

impl Capture {
    /// An empty capture that keeps at most `limit` bytes. With an
    /// `encoding`, the body is decoded and `limit` counts decoded bytes.
    pub(crate) fn new(limit: u64, encoding: Option<Encoding>) -> Self {
        Self {
            limit,
            kept: Vec::new(),
            received: 0,
            truncated: false,
            decoder: encoding.map(|encoding| Decoder::new(encoding, limit)),
        }
    }

    /// Counts `data` as received. A decoded body is decoded until its
    /// output would pass the limit, which cuts it there and marks it
    /// truncated. Any other body keeps bytes until `limit` are held, and is
    /// marked truncated when one is dropped. Bytes after a cut are only
    /// counted. A broken compressed stream fails as `protocol` in `body`.
    pub(crate) fn push(&mut self, data: &[u8]) -> Result<(), Failure> {
        self.received = self.received.saturating_add(data.len() as u64);
        let Some(decoder) = &mut self.decoder else {
            self.keep(data);
            return Ok(());
        };
        if decoder.write(data).map_err(corrupt)? {
            // A full decoder gives its output without checking the stream.
            // That output fills `kept` to the limit, so later bytes are only
            // counted.
            if let Some(decoder) = self.decoder.take() {
                (self.kept, self.truncated) = decoder.finish().map_err(corrupt)?;
            }
        }
        Ok(())
    }

    /// Keeps `data` up to the limit.
    fn keep(&mut self, data: &[u8]) {
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

    /// Takes the kept body and whether bytes were dropped. A body still
    /// being decoded is ended first, and fails as `protocol` in `body` when
    /// its stream is cut short or its check fails. A body that never got a
    /// byte is empty, and its decoder isn't asked. `received` stays readable
    /// afterwards.
    pub(crate) fn finish(&mut self) -> Result<(Vec<u8>, bool), Failure> {
        match self.decoder.take() {
            Some(decoder) if self.received > 0 => decoder.finish().map_err(corrupt),
            _ => Ok((std::mem::take(&mut self.kept), self.truncated)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::*;
    use crate::decode::Encoding;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn under_the_limit_is_kept_whole() {
        let mut capture = Capture::new(10, None);
        capture.push(b"abc").unwrap();
        assert_eq!(capture.received(), 3);
        assert_eq!(capture.finish(), Ok((b"abc".to_vec(), false)));
    }

    #[test]
    fn exactly_at_the_limit_is_not_truncated() {
        let mut capture = Capture::new(3, None);
        capture.push(b"abc").unwrap();
        assert_eq!(capture.finish(), Ok((b"abc".to_vec(), false)));
    }

    #[test]
    fn past_the_limit_is_cut_and_counted() {
        let mut capture = Capture::new(3, None);
        capture.push(b"abcd").unwrap();
        capture.push(b"ef").unwrap();
        assert_eq!(capture.received(), 6);
        assert_eq!(capture.finish(), Ok((b"abc".to_vec(), true)));
    }

    #[test]
    fn frames_crossing_the_limit() {
        let mut capture = Capture::new(5, None);
        capture.push(b"abc").unwrap();
        capture.push(b"def").unwrap();
        capture.push(b"g").unwrap();
        assert_eq!(capture.received(), 7);
        assert_eq!(capture.finish(), Ok((b"abcde".to_vec(), true)));
    }

    #[test]
    fn a_zero_limit_keeps_nothing() {
        let mut capture = Capture::new(0, None);
        capture.push(b"a").unwrap();
        assert_eq!(capture.received(), 1);
        assert_eq!(capture.finish(), Ok((Vec::new(), true)));
        let mut empty = Capture::new(0, None);
        empty.push(b"").unwrap();
        assert_eq!(empty.finish(), Ok((Vec::new(), false)));
    }

    #[test]
    fn a_zero_byte_gzip_body_is_empty() {
        let mut capture = Capture::new(10, Some(Encoding::Gzip));
        assert_eq!(capture.finish(), Ok((vec![], false)));
        assert_eq!(capture.received(), 0);
    }

    #[test]
    fn a_gzip_body_past_the_limit_is_cut_and_counted() {
        let stream = gzip(&[7; 100]);
        let (head, tail) = stream.split_at(stream.len() / 2);
        let mut capture = Capture::new(10, Some(Encoding::Gzip));
        capture.push(head).unwrap();
        capture.push(tail).unwrap();
        // Once cut, later bytes are only counted, never decoded.
        capture.push(b"not gzip").unwrap();
        assert_eq!(capture.received(), stream.len() as u64 + 8);
        assert_eq!(capture.finish(), Ok((vec![7; 10], true)));
    }

    #[test]
    fn corrupt_input_is_a_body_failure() {
        let mut capture = Capture::new(10, Some(Encoding::Gzip));
        assert_eq!(
            capture.push(b"this is not gzip at all"),
            Err(Failure {
                kind: FailureKind::Protocol,
                stage: FailureStage::Body,
                message: text::BODY_CORRUPT,
            })
        );
        assert_eq!(capture.received(), 23);
    }
}
