//! Decoding a gzip or br body as it arrives, in bounded steps, into a
//! buffer that never holds more than the body limit.
//!
//! Each decoder writes into a [`Sink`] that keeps at most `limit` bytes. A
//! write that would pass the limit keeps what fits, and the next is refused,
//! which stops the decoder. So no call keeps more than `limit` bytes, and a
//! bomb costs at most the limit of decoding, plus what the decoder runs
//! ahead of its sink: about 64 KiB for gzip, which is flate2's 32 KiB buffer
//! plus up to 32 KiB of output pending inside miniz, and up to the stream's
//! window for br, which is at most 16 MiB because the large-window
//! extension is refused.
//!
//! Decoding runs in steps, so the caller can yield between them. The sink
//! takes at most [`STEP_OUTPUT_BYTES`] in one step, and then refuses for the
//! rest of it. A step ends there: the decoder keeps the output it couldn't
//! hand over, and the input it didn't use goes back to the caller for the
//! next step. So a step hands back at most 64 KiB, but it can decode ahead
//! of the sink by the same amounts as at the limit: about 64 KiB for gzip,
//! and for br up to the stream's window, at most 16 MiB, because brotli
//! writes its ring buffer out only when it fills. A gzip step also starts at
//! most one member. Every step uses input, hands output to the sink, or
//! ends the stream, so no loop spins without progress, and a step that does
//! none of these is `Corrupt`.
//!
//! A gzip body of up to [`MAX_GZIP_MEMBERS`] members decodes as one body.
//! Each member costs a fresh decoder, and an empty member adds no output for
//! the limit to stop, so more members than that are corrupt.
//!
//! Every check here applies to what is decoded before the limit: the gzip
//! checksum and length, the brotli end marker, a broken stream, bytes after
//! the stream and the member cap. Once output passes the limit, the decoder
//! is dropped, and the rest of the body is never decoded or checked.

use std::io::{self, Write};

use brotli_decompressor::{BrotliDecompressStream, BrotliResult, BrotliState, StandardAlloc};
use flate2::write::GzDecoder;
use http::HeaderMap;
use http::header::CONTENT_ENCODING;

/// The most decoded output one step hands to the sink: 64 KiB.
pub(crate) const STEP_OUTPUT_BYTES: usize = 64 * 1024;

/// The most gzip members one body may hold. Each new member builds a fresh
/// decoder, with tens of KiB of state, for a few input bytes, and an empty
/// member adds no output, so the body limit never stops a run of them.
/// Starting one more is corrupt.
const MAX_GZIP_MEMBERS: u32 = 1024;

/// The buffer each brotli call copies its output into, on the way to the
/// sink: 16 KiB. brotli decodes into its own ring buffer, up to the
/// stream's window, and copies out from there.
const BROTLI_OUT_BYTES: usize = 16 * 1024;

/// A content coding that is decoded.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encoding {
    /// `gzip`, or its alias `x-gzip`.
    Gzip,
    /// `br`.
    Brotli,
}

/// The coding a response's body is decoded with: `Some` only for exactly one
/// `Content-Encoding` header whose value, trimmed and compared ignoring ASCII
/// case, is `gzip`, `x-gzip` or `br`. No header, `identity`, a list, several
/// headers or any other coding leaves the body raw.
pub(crate) fn encoding(headers: &HeaderMap) -> Option<Encoding> {
    let mut values = headers.get_all(CONTENT_ENCODING).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let value = value.as_bytes().trim_ascii();
    if value.eq_ignore_ascii_case(b"gzip") || value.eq_ignore_ascii_case(b"x-gzip") {
        Some(Encoding::Gzip)
    } else if value.eq_ignore_ascii_case(b"br") {
        Some(Encoding::Brotli)
    } else {
        None
    }
}

/// The compressed stream is broken, or ends early.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Corrupt;

/// What one step did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// The step used this many bytes from the start of its input. The rest
    /// of the input, and any output the decoder still holds, wait for the
    /// next step.
    Used(usize),
    /// Decoded output passed the limit: exactly `limit` bytes are kept, and
    /// nothing more should be decoded.
    Full,
}

/// Where a decoder's output goes. It keeps at most `limit` bytes, and takes
/// at most `budget` more in the current step.
#[derive(Default)]
struct Sink {
    kept: Vec<u8>,
    limit: u64,
    /// Whether a byte past the limit was refused. Every write after that
    /// fails.
    full: bool,
    /// How many more bytes the current step may take. Between steps it is
    /// 0, so the sink takes nothing.
    budget: usize,
    /// Whether a write was refused because the step's budget was spent.
    spent: bool,
}

impl Write for Sink {
    /// Keeps what fits under the limit and the step's budget. With no room
    /// left, it refuses a non-empty write with an error, never with `Ok(0)`,
    /// which would make the decoders retry forever. A refused byte past the
    /// limit marks the sink full. One within the limit but past the budget
    /// marks the step spent, and the decoder keeps it for the next step.
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let held = self.kept.len() as u64;
        let room = usize::try_from(self.limit.saturating_sub(held)).unwrap_or(usize::MAX);
        if self.full || room == 0 {
            self.full = true;
            return Err(io::ErrorKind::WriteZero.into());
        }
        if self.budget == 0 {
            self.spent = true;
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let take = data.len().min(room).min(self.budget);
        let limit = usize::try_from(self.limit).unwrap_or(usize::MAX);
        crate::capture::grow_kept(&mut self.kept, take, limit);
        self.kept.extend_from_slice(&data[..take]);
        self.budget -= take;
        Ok(take)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// What a decoder's error means: the limit when the sink refused a byte past
/// it, the end of the step, having used `used` input bytes, when the sink
/// refused a byte past the step's budget, and a broken stream otherwise.
/// Both decoders return at once when the sink refuses, so the error is that
/// refusal.
fn stopped(sink: &mut Sink, used: usize) -> Result<Step, Corrupt> {
    if sink.full {
        Ok(Step::Full)
    } else if std::mem::take(&mut sink.spent) {
        Ok(Step::Used(used))
    } else {
        Err(Corrupt)
    }
}

/// Every gzip member in turn, as one body.
struct Gzip {
    /// The decoder of the current member. Boxed, because it holds hundreds
    /// of bytes of state, and a capture lives in every hop. A finished
    /// member's decoder takes no more input, so the next member gets a fresh
    /// one over the same sink.
    decoder: Box<GzDecoder<Sink>>,
    /// How many members have started, up to [`MAX_GZIP_MEMBERS`].
    members: u32,
}

impl Gzip {
    /// Decodes from the start of `data` until the step's budget is spent,
    /// `data` is used up, or a new member has started. A `GzDecoder` returns
    /// at once when its sink refuses a write, keeping the output it couldn't
    /// hand over, and has then used none of that call's input, so the step
    /// can stop there and go on later.
    ///
    /// A step starts at most one member, because each start builds a fresh
    /// decoder whatever the member's size. It ends once the new member's
    /// first write has used input, so it never ends without progress.
    fn step(&mut self, data: &[u8]) -> Result<Step, Corrupt> {
        let mut used = 0;
        let mut started = false;
        while used < data.len() {
            if self.sink().budget == 0 {
                return Ok(Step::Used(used));
            }
            match self.decoder.write(&data[used..]) {
                // A decoder that takes none of the input has ended, and the
                // input follows its stream. It may be a next member.
                Ok(0) => {
                    if self.members >= MAX_GZIP_MEMBERS {
                        return Err(Corrupt);
                    }
                    // Hands over the ended member's last output, then checks
                    // its checksum and length.
                    if self.decoder.try_finish().is_err() {
                        return stopped(self.sink_mut(), used);
                    }
                    let sink = std::mem::take(self.decoder.get_mut());
                    *self.decoder = GzDecoder::new(sink);
                    self.members += 1;
                    started = true;
                }
                // The new member's first write reads its header, and the
                // ended member's output is all handed over, so nothing is
                // left to flush.
                Ok(taken) if started => return Ok(Step::Used(used + taken)),
                Ok(taken) => used += taken,
                Err(_) => return stopped(self.sink_mut(), used),
            }
        }
        // gzip holds back up to one buffer of output. Flushing hands the
        // sink what the budget allows, so the limit is seen in this step.
        if self.sink().budget > 0 && self.decoder.flush().is_err() {
            return stopped(self.sink_mut(), used);
        }
        Ok(Step::Used(used))
    }

    /// Hands over the output still held, within the step's budget, and then
    /// checks the checksum and length. It gives `false` while output is still
    /// held.
    fn drain(&mut self) -> Result<bool, Corrupt> {
        match self.decoder.try_finish() {
            Ok(()) => Ok(true),
            Err(_) => stopped(self.sink_mut(), 0).map(|step| step == Step::Full),
        }
    }

    fn sink(&self) -> &Sink {
        self.decoder.get_ref()
    }

    fn sink_mut(&mut self) -> &mut Sink {
        self.decoder.get_mut()
    }
}

/// A brotli stream, through the library's resumable stream call. The call
/// stops when its output space is full and goes on from there, so a step
/// gives it no more space than the step's budget. The call decodes into the
/// library's ring buffer and copies out only when that fills, so one call
/// can decode up to the stream's window before it hands back any output.
struct Brotli {
    /// Boxed, because it holds kilobytes of state.
    state: Box<BrotliState<StandardAlloc, StandardAlloc, StandardAlloc>>,
    out: Box<[u8]>,
    sink: Sink,
    /// The library's count of output bytes, which it needs.
    total_out: usize,
    /// Whether the stream's end marker was decoded.
    ended: bool,
}

impl Brotli {
    /// A stream decoder that refuses the large-window extension, which the
    /// library's other constructors accept.
    fn new(sink: Sink) -> Self {
        let state = BrotliState::new_strict(
            StandardAlloc::default(),
            StandardAlloc::default(),
            StandardAlloc::default(),
        );
        Self {
            state: Box::new(state),
            out: vec![0; BROTLI_OUT_BYTES].into_boxed_slice(),
            sink,
            total_out: 0,
            ended: false,
        }
    }

    /// One stream call over `data` from `*used`, with output space of at
    /// most the step's budget. It moves `*used` past the input taken, hands
    /// the output to the sink, and gives the call's result and how many
    /// bytes it put out. An error is the sink's refusal.
    fn call(&mut self, data: &[u8], used: &mut usize) -> io::Result<(BrotliResult, usize)> {
        let mut available_in = data.len() - *used;
        let mut available_out = self.out.len().min(self.sink.budget);
        let mut produced = 0;
        let result = BrotliDecompressStream(
            &mut available_in,
            used,
            data,
            &mut available_out,
            &mut produced,
            &mut self.out[..],
            &mut self.total_out,
            &mut *self.state,
        );
        self.sink.write_all(&self.out[..produced])?;
        Ok((result, produced))
    }

    /// Decodes from the start of `data` until the step's budget is spent,
    /// `data` is used up, or the stream ends. Bytes after the end are
    /// `Corrupt`.
    fn step(&mut self, data: &[u8]) -> Result<Step, Corrupt> {
        let mut used = 0;
        loop {
            if self.ended {
                return if used < data.len() {
                    Err(Corrupt)
                } else {
                    Ok(Step::Used(used))
                };
            }
            if self.sink.budget == 0 {
                return Ok(Step::Used(used));
            }
            let Ok((result, produced)) = self.call(data, &mut used) else {
                return stopped(&mut self.sink, used);
            };
            match result {
                BrotliResult::ResultSuccess => self.ended = true,
                // All of `data` is in. Output the call had no room for waits
                // for the next step.
                BrotliResult::NeedsMoreInput => return Ok(Step::Used(used)),
                BrotliResult::NeedsMoreOutput if produced > 0 => {}
                // Out of room without any output is no progress.
                BrotliResult::NeedsMoreOutput | BrotliResult::ResultFailure => {
                    return Err(Corrupt);
                }
            }
        }
    }

    /// Hands over the output still held, within the step's budget, until
    /// the end marker. It gives `false` while output is still held. A stream
    /// with nothing left to give and no end marker is cut short.
    fn drain(&mut self) -> Result<bool, Corrupt> {
        let mut used = 0;
        loop {
            if self.ended {
                return Ok(true);
            }
            if self.sink.budget == 0 {
                return Ok(false);
            }
            let Ok((result, produced)) = self.call(&[], &mut used) else {
                return stopped(&mut self.sink, 0).map(|step| step == Step::Full);
            };
            match result {
                BrotliResult::ResultSuccess => self.ended = true,
                BrotliResult::NeedsMoreInput | BrotliResult::NeedsMoreOutput if produced > 0 => {}
                _ => return Err(Corrupt),
            }
        }
    }
}

enum Inner {
    Gzip(Gzip),
    Brotli(Brotli),
}

/// A body being decoded under a size limit, a step at a time.
pub(crate) struct Decoder {
    inner: Inner,
    /// The most output one step hands to the sink.
    step: usize,
}

impl Decoder {
    /// A decoder for `encoding` that keeps at most `limit` decoded bytes,
    /// with steps of [`STEP_OUTPUT_BYTES`]. Brotli's large-window extension
    /// is refused, so its window is at most the 16 MiB RFC 7932 allows.
    pub(crate) fn new(encoding: Encoding, limit: u64) -> Self {
        Self::with_step(encoding, limit, STEP_OUTPUT_BYTES)
    }

    /// Like [`Decoder::new`], with steps of `step` bytes of output.
    fn with_step(encoding: Encoding, limit: u64, step: usize) -> Self {
        let sink = Sink {
            limit,
            ..Sink::default()
        };
        let inner = match encoding {
            Encoding::Gzip => Inner::Gzip(Gzip {
                decoder: Box::new(GzDecoder::new(sink)),
                members: 1,
            }),
            Encoding::Brotli => Inner::Brotli(Brotli::new(sink)),
        };
        Self { inner, step }
    }

    /// Runs one step over `data`: decodes from its start until the step has
    /// handed over its budget of output, `data` is used up, or output passes
    /// the limit. It gives how much of `data` it used, or [`Step::Full`]
    /// once output passed the limit: exactly `limit` bytes are then kept,
    /// and nothing more should be decoded.
    pub(crate) fn step(&mut self, data: &[u8]) -> Result<Step, Corrupt> {
        let before = self.sink().kept.len();
        self.open();
        let stepped = match &mut self.inner {
            Inner::Gzip(gzip) => gzip.step(data),
            Inner::Brotli(brotli) => brotli.step(data),
        };
        self.close();
        // A step that uses none of the input and hands over nothing would be
        // run again on the same input forever.
        if stepped == Ok(Step::Used(0)) && !data.is_empty() && self.sink().kept.len() == before {
            return Err(Corrupt);
        }
        stepped
    }

    /// Runs one step toward the end of the stream, once all its input is in.
    /// It gives `true` once the stream has ended and been checked, or output
    /// passed the limit, and `false` while the decoder still holds output
    /// for another step. Unless the limit was passed, the gzip checksum and
    /// length, or the brotli end marker, are checked, so a stream that is
    /// cut short is `Corrupt`.
    pub(crate) fn drain(&mut self) -> Result<bool, Corrupt> {
        if self.sink().full {
            return Ok(true);
        }
        let before = self.sink().kept.len();
        self.open();
        let drained = match &mut self.inner {
            Inner::Gzip(gzip) => gzip.drain(),
            Inner::Brotli(brotli) => brotli.drain(),
        };
        self.close();
        // A drain that hands over nothing and doesn't end would run forever.
        if drained == Ok(false) && self.sink().kept.len() == before {
            return Err(Corrupt);
        }
        drained
    }

    /// Ends the stream, and gives the decoded bytes and whether any were
    /// dropped. It drains what is left first, all at once, so after
    /// [`drain`](Self::drain) gave `true` it decodes nothing. The checks are
    /// `drain`'s.
    pub(crate) fn finish(mut self) -> Result<(Vec<u8>, bool), Corrupt> {
        while !self.drain()? {}
        let sink = self.sink_mut();
        Ok((std::mem::take(&mut sink.kept), sink.full))
    }

    /// Starts a step: the sink takes up to the step's budget.
    fn open(&mut self) {
        let step = self.step;
        self.sink_mut().budget = step;
    }

    /// Ends a step: the sink takes nothing until the next one.
    fn close(&mut self) {
        let sink = self.sink_mut();
        sink.budget = 0;
        sink.spent = false;
    }

    fn sink(&self) -> &Sink {
        match &self.inner {
            Inner::Gzip(gzip) => gzip.sink(),
            Inner::Brotli(brotli) => &brotli.sink,
        }
    }

    fn sink_mut(&mut self) -> &mut Sink {
        match &mut self.inner {
            Inner::Gzip(gzip) => gzip.sink_mut(),
            Inner::Brotli(brotli) => &mut brotli.sink,
        }
    }
}

#[cfg(test)]
mod tests {
    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::*;

    /// `hello world` as br: an uncompressed meta-block holding the text, then
    /// an empty last meta-block (RFC 7932 §9.2), with a 64 KiB window.
    const HELLO_WORLD_BR: [u8; 15] = [
        0xa0, 0x00, 0x10, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x20, 0x77, 0x6f, 0x72, 0x6c, 0x64, 0x03,
    ];

    /// `hello` as br, the same way, but with the large-window header
    /// (window bits 16) that RFC 7932 doesn't allow.
    const HELLO_LARGE_WINDOW_BR: [u8; 11] = [
        0x11, 0x10, 0x08, 0x00, 0x02, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x03,
    ];

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    /// Steps through all of `data`, as one frame. It gives `true` once
    /// output passed the limit.
    fn write(decoder: &mut Decoder, data: &[u8]) -> Result<bool, Corrupt> {
        let mut rest = data;
        while !rest.is_empty() {
            match decoder.step(rest)? {
                Step::Used(used) => rest = &rest[used..],
                Step::Full => return Ok(true),
            }
        }
        Ok(false)
    }

    /// Decodes `data` as one frame under `limit`.
    fn decode(encoding: Encoding, data: &[u8], limit: u64) -> Result<(Vec<u8>, bool), Corrupt> {
        let mut decoder = Decoder::new(encoding, limit);
        write(&mut decoder, data)?;
        decoder.finish()
    }

    /// What a stepped decode gave: the output, and how much each step
    /// handed over, the pushes' steps apart from the drain's.
    #[derive(Debug, PartialEq, Eq)]
    struct Stepped {
        kept: Vec<u8>,
        pushes: Vec<usize>,
        drains: Vec<usize>,
    }

    impl Stepped {
        /// Every step's size, the pushes' then the drain's.
        fn sizes(&self) -> impl Iterator<Item = usize> + '_ {
            self.pushes.iter().chain(&self.drains).copied()
        }
    }

    /// Decodes `frames` with steps of `step` bytes, then drains.
    fn stepped(
        encoding: Encoding,
        frames: &[&[u8]],
        limit: u64,
        step: usize,
    ) -> Result<Stepped, Corrupt> {
        let mut decoder = Decoder::with_step(encoding, limit, step);
        let mut held = 0;
        let mut grown = |decoder: &Decoder| {
            let now = decoder.sink().kept.len();
            let size = now - held;
            held = now;
            size
        };
        let mut pushes = Vec::new();
        for frame in frames {
            let mut rest = *frame;
            while !rest.is_empty() {
                let Step::Used(used) = decoder.step(rest)? else {
                    panic!("the limit is never reached here")
                };
                pushes.push(grown(&decoder));
                rest = &rest[used..];
            }
        }
        let mut drains = Vec::new();
        loop {
            let done = decoder.drain()?;
            drains.push(grown(&decoder));
            if done {
                break;
            }
        }
        let (kept, truncated) = decoder.finish()?;
        assert!(!truncated);
        Ok(Stepped {
            kept,
            pushes,
            drains,
        })
    }

    fn headers(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(CONTENT_ENCODING, value.parse().unwrap());
        }
        headers
    }

    #[test]
    fn encodings_are_chosen_exactly() {
        let chosen = [
            (&["gzip"][..], Some(Encoding::Gzip)),
            (&["x-gzip"], Some(Encoding::Gzip)),
            (&["GZIP"], Some(Encoding::Gzip)),
            (&[" br "], Some(Encoding::Brotli)),
            (&["identity"], None),
            (&["gzip, br"], None),
            (&["gzip", "gzip"], None),
            (&["zstd"], None),
            (&["deflate"], None),
            (&[], None),
        ];
        for (values, want) in chosen {
            assert!(encoding(&headers(values)) == want, "{values:?}");
        }
    }

    #[test]
    fn gzip_decodes() {
        assert_eq!(
            decode(Encoding::Gzip, &gzip(b"hello"), 10),
            Ok((b"hello".to_vec(), false))
        );
    }

    #[test]
    fn gzip_fed_one_byte_at_a_time() {
        let mut decoder = Decoder::new(Encoding::Gzip, 10);
        for byte in gzip(b"hello") {
            assert_eq!(write(&mut decoder, &[byte]), Ok(false));
        }
        assert_eq!(decoder.finish(), Ok((b"hello".to_vec(), false)));
    }

    #[test]
    fn gzip_past_the_limit_is_cut() {
        let plain: Vec<u8> = (0..100u8).collect();
        let mut decoder = Decoder::new(Encoding::Gzip, 10);
        // The frame whose output passes the limit says so.
        assert_eq!(write(&mut decoder, &gzip(&plain)), Ok(true));
        assert_eq!(decoder.finish(), Ok((plain[..10].to_vec(), true)));
    }

    #[test]
    fn a_gzip_bomb_stops_at_the_limit() {
        const LIMIT: usize = 1 << 20;
        let bomb = gzip(&vec![0; 64 << 20]);
        let mut decoder = Decoder::new(Encoding::Gzip, LIMIT as u64);
        let mut pushed = 0;
        let mut full = false;
        for chunk in bomb.chunks(1024) {
            pushed += chunk.len();
            if write(&mut decoder, chunk) == Ok(true) {
                full = true;
                break;
            }
        }
        assert!(full, "decoding never reached the limit");
        assert!(pushed < bomb.len(), "{pushed} of {} pushed", bomb.len());
        let (kept, truncated) = decoder.finish().unwrap();
        assert_eq!(kept.len(), LIMIT);
        assert!(kept.iter().all(|&byte| byte == 0));
        assert!(truncated);
    }

    #[test]
    fn exactly_at_the_limit_is_checked() {
        let mut stream = gzip(b"0123456789");
        assert_eq!(
            decode(Encoding::Gzip, &stream, 10),
            Ok((b"0123456789".to_vec(), false))
        );
        // The last byte is part of the stream's length check.
        *stream.last_mut().unwrap() ^= 1;
        assert_eq!(decode(Encoding::Gzip, &stream, 10), Err(Corrupt));
    }

    #[test]
    fn corrupt_gzip_fails() {
        assert_eq!(
            decode(Encoding::Gzip, b"this is not gzip at all", 100),
            Err(Corrupt)
        );
        let stream = gzip(b"hello, hello, hello");
        let cut = &stream[..stream.len() / 2];
        let mut decoder = Decoder::new(Encoding::Gzip, 100);
        assert_eq!(write(&mut decoder, cut), Ok(false));
        assert_eq!(decoder.finish(), Err(Corrupt));
    }

    #[test]
    fn two_gzip_members_decode_as_one() {
        let mut stream = gzip(b"ab");
        stream.extend(gzip(b"cd"));
        assert_eq!(
            decode(Encoding::Gzip, &stream, 10),
            Ok((b"abcd".to_vec(), false))
        );
    }

    #[test]
    fn up_to_the_member_cap_decodes_and_past_it_is_corrupt() {
        let empty = gzip(b"");
        let members = |count: usize| empty.repeat(count);
        assert_eq!(
            decode(Encoding::Gzip, &members(1024), 10),
            Ok((Vec::new(), false))
        );
        assert_eq!(decode(Encoding::Gzip, &members(1025), 10), Err(Corrupt));
        // Fed a member at a time, the cap is the same.
        let mut decoder = Decoder::new(Encoding::Gzip, 10);
        let mut results = Vec::new();
        for _ in 0..1025 {
            results.push(write(&mut decoder, &empty));
        }
        assert!(results[..1024].iter().all(|r| *r == Ok(false)));
        assert_eq!(results[1024], Err(Corrupt));
    }

    #[test]
    fn a_thousand_members_with_data_join() {
        let mut stream = Vec::new();
        let mut want = Vec::new();
        for n in 0..1024u32 {
            let piece = n.to_le_bytes();
            stream.extend(gzip(&piece));
            want.extend(piece);
        }
        assert_eq!(decode(Encoding::Gzip, &stream, 1 << 20), Ok((want, false)));
    }

    #[test]
    fn bytes_after_the_stream_are_corrupt() {
        let mut br = HELLO_WORLD_BR.to_vec();
        br.push(0);
        assert_eq!(decode(Encoding::Brotli, &br, 100), Err(Corrupt));
        let mut stream = gzip(b"hello");
        stream.extend(b"\0\0");
        assert_eq!(decode(Encoding::Gzip, &stream, 100), Err(Corrupt));
    }

    #[test]
    fn brotli_exactly_at_the_limit_is_checked() {
        assert_eq!(
            decode(Encoding::Brotli, &HELLO_WORLD_BR, 11),
            Ok((b"hello world".to_vec(), false))
        );
        assert_eq!(
            decode(Encoding::Brotli, &HELLO_WORLD_BR[..14], 11),
            Err(Corrupt)
        );
    }

    #[test]
    fn a_decoded_body_never_holds_more_than_the_limit() {
        const LIMIT: usize = 1000;
        let plain: Vec<u8> = (0..5000u32).map(|n| (n % 251) as u8).collect();
        let stream = gzip(&plain);
        let mut decoder = Decoder::new(Encoding::Gzip, LIMIT as u64);
        for chunk in stream.chunks(7) {
            if write(&mut decoder, chunk) == Ok(true) {
                break;
            }
        }
        let (kept, truncated) = decoder.finish().unwrap();
        assert!(truncated);
        assert_eq!(kept, plain[..LIMIT]);
        assert!(kept.capacity() <= LIMIT, "{}", kept.capacity());
    }

    #[test]
    fn brotli_decodes_and_cuts() {
        assert_eq!(
            decode(Encoding::Brotli, &HELLO_WORLD_BR, 100),
            Ok((b"hello world".to_vec(), false))
        );
        let mut decoder = Decoder::new(Encoding::Brotli, 5);
        assert_eq!(write(&mut decoder, &HELLO_WORLD_BR), Ok(true));
        assert_eq!(decoder.finish(), Ok((b"hello".to_vec(), true)));
        assert_eq!(
            decode(Encoding::Brotli, b"this is not brotli", 100),
            Err(Corrupt)
        );
        // Cut before its last meta-block, the stream has no end marker.
        assert_eq!(
            decode(Encoding::Brotli, &HELLO_WORLD_BR[..14], 100),
            Err(Corrupt)
        );
    }

    #[test]
    fn a_large_window_brotli_stream_is_corrupt() {
        // The library's own default reads large-window streams.
        let mut lenient = brotli_decompressor::DecompressorWriter::new(Vec::new(), 0);
        lenient.write_all(&HELLO_LARGE_WINDOW_BR).unwrap();
        assert_eq!(lenient.into_inner().unwrap(), b"hello");

        assert_eq!(
            decode(Encoding::Brotli, &HELLO_LARGE_WINDOW_BR, 100),
            Err(Corrupt)
        );
    }

    #[test]
    fn a_gzip_step_hands_over_at_most_its_budget() {
        const STEP: usize = 4096;
        let plain = vec![0; 1 << 20];
        let stream = gzip(&plain);
        let got = stepped(Encoding::Gzip, &[&stream], 2 << 20, STEP).unwrap();
        assert_eq!(got.kept, plain);
        assert!(got.sizes().all(|size| size <= STEP), "{got:?}");
        // The whole stream is one frame, so only steps split its output.
        assert!(got.sizes().count() >= plain.len() / STEP);
    }

    #[test]
    fn a_brotli_step_hands_over_at_most_its_budget() {
        const STEP: usize = 3;
        let got = stepped(Encoding::Brotli, &[&HELLO_WORLD_BR], 100, STEP).unwrap();
        assert_eq!(got.kept, b"hello world");
        assert!(got.sizes().all(|size| size <= STEP), "{got:?}");
        // The first push takes all 15 input bytes and hands over 3 of the 11
        // output bytes. The rest is pending in the decoder, and the drain
        // hands it over in steps: 3, 3 and 2 here.
        let drained = got.drains.iter().filter(|&&size| size > 0).count();
        assert!(drained >= 2, "{got:?}");
    }

    #[test]
    fn each_gzip_member_starts_a_step() {
        let empty = gzip(b"");
        let got = stepped(Encoding::Gzip, &[&empty.repeat(100)], 10, STEP_OUTPUT_BYTES).unwrap();
        assert!(got.kept.is_empty());
        // The first member has the decoder's own start, and each of the
        // other 99 ends the step that starts it, so one frame takes 100.
        assert!(got.pushes.len() >= 100, "{} steps", got.pushes.len());
        // With data, the members still join.
        let mut stream = Vec::new();
        for piece in [&b"ab"[..], b"cd", b"ef"] {
            stream.extend(gzip(piece));
        }
        let got = stepped(Encoding::Gzip, &[&stream], 10, STEP_OUTPUT_BYTES).unwrap();
        assert_eq!(got.kept, b"abcdef");
        assert!(got.pushes.len() >= 3, "{} steps", got.pushes.len());
    }

    #[test]
    fn many_small_steps_give_the_same_bytes_as_one() {
        let plain: Vec<u8> = (0..300_000u32).map(|n| (n / 7 % 13) as u8).collect();
        let mut stream = gzip(&plain);
        stream.extend(gzip(b"and a second member"));
        let mut want = plain.clone();
        want.extend(b"and a second member");
        let frames: Vec<&[u8]> = stream.chunks(7).collect();
        let got = stepped(Encoding::Gzip, &frames, 1 << 20, 1000).unwrap();
        assert_eq!(got.kept, want);
        assert!(got.sizes().all(|size| size <= 1000));
        assert_eq!(decode(Encoding::Gzip, &stream, 1 << 20), Ok((want, false)));

        let frames: Vec<&[u8]> = HELLO_WORLD_BR.chunks(1).collect();
        let got = stepped(Encoding::Brotli, &frames, 100, 1).unwrap();
        assert_eq!(got.kept, b"hello world");
    }

    #[test]
    fn stepped_decoding_keeps_its_checks() {
        // Cut short, a stream fails in the drain however small the steps.
        let stream = gzip(&[5; 10_000]);
        let cut = &stream[..stream.len() - 1];
        assert_eq!(stepped(Encoding::Gzip, &[cut], 1 << 20, 100), Err(Corrupt));
        assert_eq!(
            stepped(Encoding::Brotli, &[&HELLO_WORLD_BR[..14]], 100, 2),
            Err(Corrupt)
        );
        // Bytes after the end are still found.
        let mut br = HELLO_WORLD_BR.to_vec();
        br.push(0);
        assert_eq!(stepped(Encoding::Brotli, &[&br], 100, 2), Err(Corrupt));
    }
}
