//! Decoding a gzip or br body as it arrives, into a buffer that never holds
//! more than the body limit.
//!
//! Each decoder writes into a [`Sink`] that keeps at most `limit` bytes. A
//! write that would pass the limit keeps what fits, and the next is refused,
//! which stops the decoder. So no call keeps more than `limit` bytes, and a
//! bomb costs at most the limit of decoding, plus what the decoder runs
//! ahead of its sink: up to 32 KiB for gzip, and up to the stream's window
//! for br, which is at most 16 MiB because the large-window extension is
//! refused.
//!
//! A gzip body of up to [`MAX_GZIP_MEMBERS`] members decodes as one body.
//! Each member costs a fresh decoder, and an empty member adds no output for
//! the limit to stop, so more members than that are corrupt.

use std::io::{self, Write};

use brotli_decompressor::{BrotliDecoderParameter, DecompressorWriter};
use flate2::write::GzDecoder;
use http::HeaderMap;
use http::header::CONTENT_ENCODING;

/// The most gzip members one body may hold. Each new member builds a fresh
/// decoder, with tens of KiB of state, for a few input bytes, and an empty
/// member adds no output, so the body limit never stops a run of them.
/// Starting one more is corrupt.
const MAX_GZIP_MEMBERS: u32 = 1024;

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

/// Where a decoder's output goes. It keeps at most `limit` bytes.
#[derive(Default)]
struct Sink {
    kept: Vec<u8>,
    limit: u64,
    /// Whether a byte past the limit was refused. Every write after that
    /// fails.
    full: bool,
}

impl Write for Sink {
    /// Keeps what fits under the limit. With no room left, it refuses a
    /// non-empty write with an error, never with `Ok(0)`, which would make
    /// the decoders retry forever.
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
        let take = data.len().min(room);
        let limit = usize::try_from(self.limit).unwrap_or(usize::MAX);
        crate::capture::grow_kept(&mut self.kept, take, limit);
        self.kept.extend_from_slice(&data[..take]);
        Ok(take)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Both are boxed, because each holds hundreds of bytes of state or more,
/// and a capture lives in every hop.
enum Inner {
    /// Every gzip member in turn, as one body: the decoder of the current
    /// member, and how many members have started. A finished member's
    /// decoder takes no more input, so the next member gets a fresh one
    /// over the same sink, up to [`MAX_GZIP_MEMBERS`].
    Gzip(Box<GzDecoder<Sink>>, u32),
    Brotli(Box<DecompressorWriter<Sink>>),
}

/// A body being decoded under a size limit.
pub(crate) struct Decoder {
    inner: Inner,
}

impl Decoder {
    /// A decoder for `encoding` that keeps at most `limit` decoded bytes.
    /// Brotli's large-window extension is refused, so its window is at most
    /// the 16 MiB RFC 7932 allows.
    pub(crate) fn new(encoding: Encoding, limit: u64) -> Self {
        let sink = Sink {
            kept: Vec::new(),
            limit,
            full: false,
        };
        let inner = match encoding {
            Encoding::Gzip => Inner::Gzip(Box::new(GzDecoder::new(sink)), 1),
            Encoding::Brotli => {
                // 0 picks the decoder's default output buffer, 4 KiB.
                let mut decoder = DecompressorWriter::new(sink, 0);
                // Before the first write, this can't be refused.
                decoder.set_parameter(BrotliDecoderParameter::BROTLI_DECODER_PARAM_LARGE_WINDOW, 0);
                Inner::Brotli(Box::new(decoder))
            }
        };
        Self { inner }
    }

    /// Decodes `data`. It gives `true` once decoded output would pass the
    /// limit: exactly `limit` bytes are then kept, the rest is dropped, and
    /// nothing more should be written.
    pub(crate) fn write(&mut self, data: &[u8]) -> Result<bool, Corrupt> {
        let mut rest = data;
        while !rest.is_empty() {
            let written = match &mut self.inner {
                Inner::Gzip(decoder, _) => decoder.write(rest),
                Inner::Brotli(decoder) => decoder.write(rest),
            };
            match written {
                // A decoder that takes none of the input has ended, and the
                // input follows its stream. Only gzip may go on, with a
                // next member.
                Ok(0) => {
                    if !self.next_gzip_member()? {
                        return Ok(true);
                    }
                }
                Ok(taken) => rest = &rest[taken..],
                Err(_) => return self.stopped(),
            }
        }
        // gzip holds back up to one buffer of output. Flushing hands it to
        // the sink, so the limit is seen on this write.
        let flushed = match &mut self.inner {
            Inner::Gzip(decoder, _) => decoder.flush(),
            Inner::Brotli(decoder) => decoder.flush(),
        };
        if flushed.is_err() {
            return self.stopped();
        }
        Ok(self.sink().full)
    }

    /// Starts the next gzip member after the current one ended, over the
    /// same sink. It gives `false` when the sink refused a byte while the
    /// ended member was finished. A brotli stream has no next member, and a
    /// gzip body at the cap, or with a broken member, is `Corrupt`.
    fn next_gzip_member(&mut self) -> Result<bool, Corrupt> {
        let Inner::Gzip(decoder, members) = &mut self.inner else {
            return Err(Corrupt);
        };
        if *members >= MAX_GZIP_MEMBERS {
            return Err(Corrupt);
        }
        if decoder.try_finish().is_err() {
            return self.stopped().map(|full| !full);
        }
        let sink = std::mem::take(decoder.get_mut());
        **decoder = GzDecoder::new(sink);
        *members += 1;
        Ok(true)
    }

    /// Ends the stream, and gives the decoded bytes and whether any were
    /// dropped. Unless the limit was passed, the gzip checksum and length, or
    /// the brotli end marker, are checked, so a stream that is cut short is
    /// `Corrupt`.
    pub(crate) fn finish(mut self) -> Result<(Vec<u8>, bool), Corrupt> {
        if !self.sink().full {
            let ended = match &mut self.inner {
                Inner::Gzip(decoder, _) => decoder.try_finish(),
                Inner::Brotli(decoder) => decoder.close(),
            };
            if ended.is_err() {
                self.stopped()?;
            }
        }
        let sink = self.sink_mut();
        Ok((std::mem::take(&mut sink.kept), sink.full))
    }

    /// What a decoder's error means: the limit when the sink refused a byte,
    /// and a broken stream otherwise.
    fn stopped(&self) -> Result<bool, Corrupt> {
        if self.sink().full {
            Ok(true)
        } else {
            Err(Corrupt)
        }
    }

    fn sink(&self) -> &Sink {
        match &self.inner {
            Inner::Gzip(decoder, _) => decoder.get_ref(),
            Inner::Brotli(decoder) => decoder.get_ref(),
        }
    }

    fn sink_mut(&mut self) -> &mut Sink {
        match &mut self.inner {
            Inner::Gzip(decoder, _) => decoder.get_mut(),
            Inner::Brotli(decoder) => decoder.get_mut(),
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

    /// Decodes `data` in one write under `limit`.
    fn decode(encoding: Encoding, data: &[u8], limit: u64) -> Result<(Vec<u8>, bool), Corrupt> {
        let mut decoder = Decoder::new(encoding, limit);
        decoder.write(data)?;
        decoder.finish()
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
            assert_eq!(decoder.write(&[byte]), Ok(false));
        }
        assert_eq!(decoder.finish(), Ok((b"hello".to_vec(), false)));
    }

    #[test]
    fn gzip_past_the_limit_is_cut() {
        let plain: Vec<u8> = (0..100u8).collect();
        let mut decoder = Decoder::new(Encoding::Gzip, 10);
        // The write whose output passes the limit says so.
        assert_eq!(decoder.write(&gzip(&plain)), Ok(true));
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
            if decoder.write(chunk) == Ok(true) {
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
        assert_eq!(decoder.write(cut), Ok(false));
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
            results.push(decoder.write(&empty));
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
            if decoder.write(chunk) == Ok(true) {
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
        assert_eq!(decoder.write(&HELLO_WORLD_BR), Ok(true));
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
}
