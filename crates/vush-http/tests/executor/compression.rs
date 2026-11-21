//! Bodies under `http.decompress`: what is asked for, what is decoded, and
//! what is kept raw.

use std::future::pending;
use std::io::{self, Write as _};
use std::net::SocketAddr;
use std::path::Path;

use bytes::Bytes;
use flate2::Compression;
use flate2::write::GzEncoder;
use http::Request;
use http::header::ACCEPT_ENCODING;
use hyper::body::Incoming;
use rustls::RootCertStore;
use tokio::net::TcpStream;
use vush_core::plan::{HttpSettings, HttpVersion, ResourceLimits};
use vush_core::protocol::http::WireVersion;
use vush_core::protocol::{FailureKind, FailureStage};
use vush_http::Executor;

use crate::common::{
    BoxFuture, MIB, NO_DIR, failure, get, header_values, method, pairs, reply, response, send,
    settings, spawn_auto, spawn_raw,
};

const HELLO: &[u8] = b"hello world";

/// `hello world` as br: an uncompressed meta-block holding the text, then an
/// empty last meta-block (RFC 7932 §9.2).
const HELLO_BR: [u8; 15] = [
    0xa0, 0x00, 0x10, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x20, 0x77, 0x6f, 0x72, 0x6c, 0x64, 0x03,
];

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// A lone executor with a 1 MiB body limit and `decompress` as given.
fn decoding(version: HttpVersion, decompress: bool) -> Executor {
    let settings = HttpSettings {
        decompress,
        ..settings(version, MIB)
    };
    Executor::with_roots(
        &settings,
        &ResourceLimits::default(),
        Path::new(NO_DIR),
        RootCertStore::empty(),
    )
    .unwrap()
}

/// Serves HTTP/1.1 and h2c. It answers every request `200` with `body`,
/// labelled with the `Content-Encoding` `encoding`, and with one
/// `x-seen-accept-encoding` header for each `Accept-Encoding` value the
/// request carried.
async fn serve(encoding: &'static str, body: impl Into<Bytes>) -> SocketAddr {
    let body = body.into();
    spawn_auto(move |request: Request<Incoming>| {
        let body = body.clone();
        async move {
            let seen: Vec<String> = request
                .headers()
                .get_all(ACCEPT_ENCODING)
                .iter()
                .map(|value| value.to_str().unwrap().to_owned())
                .collect();
            let mut headers = vec![("content-encoding", encoding)];
            headers.extend(
                seen.iter()
                    .map(|value| ("x-seen-accept-encoding", value.as_str())),
            );
            reply(200, &headers, body)
        }
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn gzip_and_br_bodies_are_decoded() {
    for (version, wire) in [
        (HttpVersion::Http1, WireVersion::Http11),
        (HttpVersion::Http2, WireVersion::Http2),
    ] {
        for (encoding, body) in [("gzip", gzip(HELLO)), ("br", HELLO_BR.to_vec())] {
            let addr = serve(encoding, body).await;
            let executor = decoding(version, true);
            let hop = send(&executor, &get(format!("http://{addr}/"))).await;
            let got = response(&hop);
            assert_eq!(got.version, wire, "{encoding}");
            assert_eq!(got.body, HELLO, "{wire:?} {encoding}");
            assert!(!got.truncated, "{wire:?} {encoding}");
            assert_eq!(
                header_values(got, "content-encoding"),
                [encoding.as_bytes()],
                "{wire:?} {encoding}"
            );
            assert_eq!(
                header_values(got, "x-seen-accept-encoding"),
                [b"gzip, br"],
                "{wire:?} {encoding}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_requests_own_accept_encoding_is_sent() {
    let addr = serve("gzip", gzip(HELLO)).await;
    let mut request = get(format!("http://{addr}/"));
    request.headers = pairs(&[("Accept-Encoding", "gzip")]);
    let hop = send(&decoding(HttpVersion::Http1, true), &request).await;
    let got = response(&hop);
    assert_eq!(header_values(got, "x-seen-accept-encoding"), [b"gzip"]);
    assert_eq!(got.body, HELLO);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bomb_is_kept_at_the_limit() {
    let bomb = gzip(&vec![0; 16 << 20]);
    let compressed = bomb.len() as u64;
    let addr = serve("gzip", bomb).await;
    let executor = decoding(HttpVersion::Http1, true);
    let mut seen = Vec::new();
    for _ in 0..2 {
        let hop = send(&executor, &get(format!("http://{addr}/"))).await;
        let got = response(&hop);
        assert_eq!(got.body.len() as u64, MIB);
        assert!(got.body.iter().all(|&byte| byte == 0));
        assert!(got.truncated);
        assert_eq!(hop.sizes.received, compressed);
        seen.push(hop.connection.expect("a connection"));
    }
    assert_eq!(seen[0], seen[1], "the second hop reuses the connection");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_corrupt_body_is_a_protocol_failure() {
    let addr = serve("gzip", &b"this is not gzip at all"[..]).await;
    let hop = send(
        &decoding(HttpVersion::Http1, true),
        &get(format!("http://{addr}/")),
    )
    .await;
    assert_eq!(failure(&hop), (FailureKind::Protocol, FailureStage::Body));
    let message = hop.outcome.unwrap_err().message;
    assert_eq!(message, "the compressed body is corrupt");
}

#[tokio::test(flavor = "multi_thread")]
async fn other_encodings_are_kept_raw() {
    let raw = [0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x01, 0xff, 0x7f];
    let addr = serve("zstd", raw.to_vec()).await;
    let hop = send(
        &decoding(HttpVersion::Http1, true),
        &get(format!("http://{addr}/")),
    )
    .await;
    let got = response(&hop);
    assert_eq!(got.body, raw);
    assert!(!got.truncated);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_decompress_nothing_changes() {
    let stream = gzip(HELLO);
    let addr = serve("gzip", stream.clone()).await;
    let hop = send(
        &decoding(HttpVersion::Http1, false),
        &get(format!("http://{addr}/")),
    )
    .await;
    let got = response(&hop);
    assert!(header_values(got, "x-seen-accept-encoding").is_empty());
    assert_eq!(got.body, stream);
}

/// Reads until the end of a request head. It gives `false` if the stream
/// ends first.
async fn read_head(stream: &TcpStream) -> bool {
    let mut seen = Vec::new();
    let mut buf = [0; 4096];
    while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
        if stream.readable().await.is_err() {
            return false;
        }
        match stream.try_read(&mut buf) {
            Ok(0) => return false,
            Ok(read) => seen.extend_from_slice(&buf[..read]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => return false,
        }
    }
    true
}

/// Answers a `HEAD` with a head that says `gzip` and 20 bytes, and no body,
/// then holds the connection open.
fn gzip_head(stream: TcpStream) -> BoxFuture {
    Box::pin(async move {
        if !read_head(&stream).await {
            return;
        }
        let mut head: &[u8] =
            b"HTTP/1.1 200 OK\r\ncontent-encoding: gzip\r\ncontent-length: 20\r\n\r\n";
        while !head.is_empty() {
            if stream.writable().await.is_err() {
                return;
            }
            match stream.try_write(head) {
                Ok(written) => head = &head[written..],
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => return,
            }
        }
        pending::<()>().await;
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_head_response_labelled_gzip_is_empty() {
    let addr = spawn_raw(gzip_head).await;
    let mut request = get(format!("http://{addr}/"));
    request.method = method("HEAD");
    let hop = send(&decoding(HttpVersion::Http1, true), &request).await;
    let got = response(&hop);
    assert_eq!(got.status, 200);
    assert!(got.body.is_empty());
    assert!(!got.truncated);
}
