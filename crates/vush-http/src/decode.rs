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

use std::io::{self, Write};

use brotli_decompressor::{BrotliDecoderParameter, DecompressorWriter};
use flate2::write::MultiGzDecoder;
use http::HeaderMap;
use http::header::CONTENT_ENCODING;

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
    /// Every gzip member in turn, as one body.
    Gzip(Box<MultiGzDecoder<Sink>>),
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
            Encoding::Gzip => Inner::Gzip(Box::new(MultiGzDecoder::new(sink))),
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
                Inner::Gzip(decoder) => decoder.write(rest),
                Inner::Brotli(decoder) => decoder.write(rest),
            };
            match written {
                // A decoder that takes none of the input has ended, and the
                // input follows its stream.
                Ok(0) => return Err(Corrupt),
                Ok(taken) => rest = &rest[taken..],
                Err(_) => return self.stopped(),
            }
        }
        // gzip holds back up to one buffer of output. Flushing hands it to
        // the sink, so the limit is seen on this write.
        let flushed = match &mut self.inner {
            Inner::Gzip(decoder) => decoder.flush(),
            Inner::Brotli(decoder) => decoder.flush(),
        };
        if flushed.is_err() {
            return self.stopped();
        }
        Ok(self.sink().full)
    }

    /// Ends the stream, and gives the decoded bytes and whether any were
    /// dropped. Unless the limit was passed, the gzip checksum and length, or
    /// the brotli end marker, are checked, so a stream that is cut short is
    /// `Corrupt`.
    pub(crate) fn finish(mut self) -> Result<(Vec<u8>, bool), Corrupt> {
        if !self.sink().full {
            let ended = match &mut self.inner {
                Inner::Gzip(decoder) => decoder.try_finish(),
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
            Inner::Gzip(decoder) => decoder.get_ref(),
            Inner::Brotli(decoder) => decoder.get_ref(),
        }
    }

    fn sink_mut(&mut self) -> &mut Sink {
        match &mut self.inner {
            Inner::Gzip(decoder) => decoder.get_mut(),
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
