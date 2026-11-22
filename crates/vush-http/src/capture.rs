//! The body of one response, as it is read: every byte is counted, and at
//! most `limit` are kept. A body with a decoded encoding is decoded as it
//! arrives, and then the limit counts decoded bytes.
//!
//! Decoding runs in steps, so the reader can yield between them:
//! [`Capture::push`] runs one step over a frame and gives back what it
//! used, and [`Capture::drain`] runs one step of the end. A step hands back
//! at most 64 KiB of output, but a br step can decode up to the stream's
//! window, at most 16 MiB, first, because brotli writes its ring buffer out
//! only when it fills.
//! Once decoded output passes the limit, the body is cut there, and the rest
//! is counted but never decoded: the checksum, end marker, corruption,
//! trailing-byte and member-cap checks apply only to what came before the
//! cut.

use vush_core::protocol::{Failure, FailureKind, FailureStage};

use crate::decode::{Corrupt, Decoder, Encoding, Step};
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

/// Makes room in `kept` for `take` more bytes, growing its capacity
/// geometrically but never past `limit`. Plain `extend_from_slice` doubles,
/// so a body near the limit could hold about twice that. The caller keeps
/// `kept.len() + take` within `limit`.
pub(crate) fn grow_kept(kept: &mut Vec<u8>, take: usize, limit: usize) {
    if kept.capacity() - kept.len() < take {
        let want = (kept.capacity().saturating_mul(2))
            .max(kept.len() + take)
            .min(limit);
        kept.reserve_exact(want - kept.len());
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

    /// Counts a frame of `len` bytes as received. A frame counts when it
    /// arrives, before any of it is pushed, so one whose decoding a timeout
    /// cuts short still counts whole.
    pub(crate) fn arrive(&mut self, len: usize) {
        self.received = self.received.saturating_add(len as u64);
    }

    /// Whether a push runs a decoding step, after which the reader should
    /// yield. A raw body, or a decoded one already cut, needs none.
    pub(crate) fn decodes(&self) -> bool {
        self.decoder.is_some()
    }

    /// Takes bytes of a frame from the start of `data`, and gives how many
    /// it took. A body that isn't decoded takes all of `data`: it keeps
    /// bytes until `limit` are held, and is marked truncated when one is
    /// dropped. A decoded body runs one step, and takes what the step used.
    /// When its output would pass the limit, it is cut there and marked
    /// truncated, and from then on every push takes all of `data` without
    /// decoding it. A broken compressed stream fails as `protocol` in
    /// `body`. Taking bytes doesn't count them: [`Capture::arrive`] does.
    pub(crate) fn push(&mut self, data: &[u8]) -> Result<usize, Failure> {
        let Some(decoder) = &mut self.decoder else {
            self.keep(data);
            return Ok(data.len());
        };
        match decoder.step(data).map_err(corrupt)? {
            Step::Used(used) => Ok(used),
            Step::Full => {
                // A full decoder gives its output without checking the
                // stream. That output fills `kept` to the limit, so later
                // bytes are only counted.
                if let Some(decoder) = self.decoder.take() {
                    (self.kept, self.truncated) = decoder.finish().map_err(corrupt)?;
                }
                Ok(data.len())
            }
        }
    }

    /// Runs one step of ending a decoded body, once its last frame is
    /// pushed. It gives `true` once nothing is left to decode, after which
    /// [`Capture::finish`] decodes nothing, and `false` while output is still
    /// held for another step. A body that isn't decoded, is already cut, or
    /// never got a byte has nothing to end. It fails as `protocol` in `body`
    /// when the stream is cut short or its check fails.
    pub(crate) fn drain(&mut self) -> Result<bool, Failure> {
        match &mut self.decoder {
            Some(decoder) if self.received > 0 => decoder.drain().map_err(corrupt),
            _ => Ok(true),
        }
    }

    /// Keeps `data` up to the limit.
    fn keep(&mut self, data: &[u8]) {
        let held = self.kept.len() as u64;
        let room = usize::try_from(self.limit.saturating_sub(held)).unwrap_or(usize::MAX);
        if data.len() > room {
            self.truncated = true;
        }
        let take = data.len().min(room);
        let limit = usize::try_from(self.limit).unwrap_or(usize::MAX);
        grow_kept(&mut self.kept, take, limit);
        self.kept.extend_from_slice(&data[..take]);
    }

    /// Every byte of every frame that arrived, kept or not.
    pub(crate) fn received(&self) -> u64 {
        self.received
    }

    /// Takes the kept body and whether bytes were dropped. A body still
    /// being decoded is ended first, with what is left of
    /// [`Capture::drain`] run all at once, and fails as `protocol` in `body`
    /// when its stream is cut short or its check fails. A body that never
    /// got a byte is empty, and its decoder isn't asked. `received` stays
    /// readable afterwards.
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

    /// Takes `frame` the way the reader does: counts it, then pushes until
    /// all of it is taken.
    fn feed(capture: &mut Capture, frame: &[u8]) -> Result<(), Failure> {
        capture.arrive(frame.len());
        let mut rest = frame;
        while !rest.is_empty() {
            rest = &rest[capture.push(rest)?..];
        }
        Ok(())
    }

    #[test]
    fn under_the_limit_is_kept_whole() {
        let mut capture = Capture::new(10, None);
        feed(&mut capture, b"abc").unwrap();
        assert_eq!(capture.received(), 3);
        assert_eq!(capture.finish(), Ok((b"abc".to_vec(), false)));
    }

    #[test]
    fn exactly_at_the_limit_is_not_truncated() {
        let mut capture = Capture::new(3, None);
        feed(&mut capture, b"abc").unwrap();
        assert_eq!(capture.finish(), Ok((b"abc".to_vec(), false)));
    }

    #[test]
    fn past_the_limit_is_cut_and_counted() {
        let mut capture = Capture::new(3, None);
        feed(&mut capture, b"abcd").unwrap();
        feed(&mut capture, b"ef").unwrap();
        assert_eq!(capture.received(), 6);
        assert_eq!(capture.finish(), Ok((b"abc".to_vec(), true)));
    }

    #[test]
    fn frames_crossing_the_limit() {
        let mut capture = Capture::new(5, None);
        feed(&mut capture, b"abc").unwrap();
        feed(&mut capture, b"def").unwrap();
        feed(&mut capture, b"g").unwrap();
        assert_eq!(capture.received(), 7);
        assert_eq!(capture.finish(), Ok((b"abcde".to_vec(), true)));
    }

    #[test]
    fn a_zero_limit_keeps_nothing() {
        let mut capture = Capture::new(0, None);
        feed(&mut capture, b"a").unwrap();
        assert_eq!(capture.received(), 1);
        assert_eq!(capture.finish(), Ok((Vec::new(), true)));
        let mut empty = Capture::new(0, None);
        feed(&mut empty, b"").unwrap();
        assert_eq!(empty.finish(), Ok((Vec::new(), false)));
    }

    #[test]
    fn a_kept_body_never_holds_more_than_the_limit() {
        const LIMIT: usize = 1000;
        let mut capture = Capture::new(LIMIT as u64, None);
        for size in [1, 3, 10, 77, 200, 333, 5, 400, 600] {
            feed(&mut capture, &vec![9; size]).unwrap();
        }
        let (kept, truncated) = capture.finish().unwrap();
        assert!(truncated);
        assert_eq!(kept.len(), LIMIT);
        assert!(kept.capacity() <= LIMIT, "{}", kept.capacity());
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
        feed(&mut capture, head).unwrap();
        feed(&mut capture, tail).unwrap();
        // Once cut, later bytes are only counted, never decoded.
        feed(&mut capture, b"not gzip").unwrap();
        assert_eq!(capture.received(), stream.len() as u64 + 8);
        assert_eq!(capture.finish(), Ok((vec![7; 10], true)));
    }

    #[test]
    fn a_frame_counts_whole_when_it_arrives() {
        // 16 MiB of output is many steps, so one push takes only part of
        // the frame, and the frame already counts whole.
        let bomb = gzip(&vec![0; 16 << 20]);
        let mut capture = Capture::new(16 << 20, Some(Encoding::Gzip));
        capture.arrive(bomb.len());
        let used = capture.push(&bomb).unwrap();
        assert!(used < bomb.len(), "{used} of {}", bomb.len());
        assert_eq!(capture.received(), bomb.len() as u64);
    }

    #[test]
    fn corrupt_input_is_a_body_failure() {
        let mut capture = Capture::new(10, Some(Encoding::Gzip));
        assert_eq!(
            feed(&mut capture, b"this is not gzip at all"),
            Err(Failure {
                kind: FailureKind::Protocol,
                stage: FailureStage::Body,
                message: text::BODY_CORRUPT,
            })
        );
        assert_eq!(capture.received(), 23);
    }
}
