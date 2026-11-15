//! Hops that fail, each from a real cause, with the exact kind and stage.
//! None of them may land on `other`.

use std::sync::Arc;
use std::time::Duration;

use vush_core::plan::HttpVersion;
use vush_core::protocol::http::{HttpBody, HttpRequest};
use vush_core::protocol::{Failure, FailureKind, FailureStage};

use crate::common::{
    CountingListener, LOCALHOST, MIB, TestCa, close_after_head, echo, executor, failure,
    garbage_status, get, h2_reset_after_one_frame, hold, malformed_chunk, pairs, refused_port,
    send, send_within, silent_after_head, spawn_auto, spawn_raw, spawn_tls, stalled_body, trusting,
};

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_port_is_connect() {
    let refused = refused_port();
    let hop = send(
        &executor(HttpVersion::Auto),
        &get(format!("http://{}/", refused.addr)),
    )
    .await;
    assert_eq!(failure(&hop), (FailureKind::Connect, FailureStage::Connect));
    assert_eq!(hop.connection, None);
    assert_eq!(hop.timing.setup, None);
    assert_eq!(hop.timing.to_headers, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_handshake_that_never_finishes_times_out_in_connect() {
    let addr = spawn_raw(hold).await;
    let executor = trusting(&TestCa::generate(), HttpVersion::Auto);
    let hop = send_within(
        &executor,
        &get(format!("https://{addr}/")),
        Duration::from_millis(500),
    )
    .await;
    assert_eq!(failure(&hop), (FailureKind::Timeout, FailureStage::Connect));
    assert_eq!(hop.connection, None);
}

/// Under `http2`, hyper-util opens one connection per origin at a time. The
/// second hop starts once the first hop's connect has reached the server, so
/// it never runs the connector: it waits for that connect, which never
/// finishes. It runs out of time long before the first hop does, while it is
/// still waiting for a connection.
#[tokio::test(flavor = "multi_thread")]
async fn a_hop_waiting_for_a_shared_http2_connect_times_out_in_connect() {
    let listener = CountingListener::bind().await;
    let url = format!("https://{}/", listener.addr);
    let executor = Arc::new(trusting(&TestCa::generate(), HttpVersion::Http2));
    let first = {
        let executor = executor.clone();
        let request = get(url.clone());
        tokio::spawn(async move { send(&executor, &request).await })
    };
    // The first hop opens the shared connect before it reaches the server.
    // Holding its connection keeps the handshake waiting.
    let _held = listener.arrival().await;
    let hop = send_within(&executor, &get(url), Duration::from_millis(300)).await;
    assert_eq!(failure(&hop), (FailureKind::Timeout, FailureStage::Connect));
    assert_eq!(hop.connection, None);
    first.abort();
}

/// When the connect that hops share under `http2` fails, hyper-util cancels
/// every hop waiting for it. Those hops fail as `connect`, never `other`.
///
/// A hop can start just after the shared connect failed. It then opens a
/// connect of its own, which fails as `tls` too, so more than one hop can
/// fail as `tls`.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_shared_http2_connect_fails_every_waiter_as_connect() {
    let leaf = TestCa::generate().leaf_for_ip(LOCALHOST);
    let addr = spawn_tls(echo, &[b"h2"], None, leaf).await;
    let executor = Arc::new(trusting(&TestCa::generate(), HttpVersion::Http2));
    let hops = [(); 6].map(|()| {
        let executor = executor.clone();
        let request = get(format!("https://{addr}/"));
        tokio::spawn(async move { send(&executor, &request).await })
    });
    let shared = Failure {
        kind: FailureKind::Connect,
        stage: FailureStage::Connect,
        message: "the shared connection attempt failed",
    };
    let mut handshakes = 0;
    for hop in hops {
        let hop = hop.await.unwrap();
        assert_eq!(hop.connection, None);
        let error = hop.outcome.as_ref().unwrap_err();
        if error.kind == FailureKind::Tls {
            assert_eq!(error.stage, FailureStage::Connect);
            handshakes += 1;
        } else {
            assert_eq!(*error, shared);
        }
    }
    assert!(
        handshakes >= 1,
        "the hop that opened the connect fails as tls"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_head_times_out_in_headers() {
    let addr = spawn_raw(silent_after_head).await;
    let hop = send_within(
        &executor(HttpVersion::Auto),
        &get(format!("http://{addr}/")),
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(failure(&hop), (FailureKind::Timeout, FailureStage::Headers));
    assert!(hop.connection.is_some());
    assert!(hop.timing.setup.is_some());
    assert_eq!(hop.timing.to_headers, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_body_times_out_in_body() {
    let addr = spawn_raw(stalled_body).await;
    let hop = send_within(
        &executor(HttpVersion::Auto),
        &get(format!("http://{addr}/")),
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(failure(&hop), (FailureKind::Timeout, FailureStage::Body));
    assert!(hop.timing.to_headers.is_some());
    assert!(hop.connection.is_some());
    assert_eq!(hop.sizes.received, 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_garbage_status_line_is_protocol() {
    let addr = spawn_raw(garbage_status).await;
    let hop = send(
        &executor(HttpVersion::Auto),
        &get(format!("http://{addr}/")),
    )
    .await;
    assert_eq!(
        failure(&hop),
        (FailureKind::Protocol, FailureStage::Headers)
    );
    assert!(hop.connection.is_some());
}

/// hyper reads a bad chunk size line as an I/O error of the body.
#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_chunked_body_is_protocol() {
    let addr = spawn_raw(malformed_chunk).await;
    let hop = send(
        &executor(HttpVersion::Auto),
        &get(format!("http://{addr}/")),
    )
    .await;
    assert_eq!(failure(&hop), (FailureKind::Protocol, FailureStage::Body));
    assert!(hop.timing.to_headers.is_some());
    assert!(hop.connection.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_early_close_is_connection_closed() {
    let addr = spawn_raw(close_after_head).await;
    let hop = send(
        &executor(HttpVersion::Auto),
        &get(format!("http://{addr}/")),
    )
    .await;
    assert_eq!(
        failure(&hop),
        (FailureKind::ConnectionClosed, FailureStage::Headers)
    );
    assert!(hop.connection.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_http2_reset_mid_body_is_connection_closed() {
    let addr = spawn_raw(h2_reset_after_one_frame).await;
    let hop = send(
        &executor(HttpVersion::Http2),
        &get(format!("http://{addr}/")),
    )
    .await;
    assert_eq!(
        failure(&hop),
        (FailureKind::ConnectionClosed, FailureStage::Body)
    );
    assert!(hop.timing.to_headers.is_some());
    assert_eq!(hop.sizes.received, 5);
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_requests_send_nothing() {
    let listener = CountingListener::bind().await;
    let addr = listener.addr;
    let ftp = get(format!("ftp://{addr}/"));
    let mut control = get(format!("http://{addr}/"));
    control.headers = pairs(&[("X-Bad", "a\r\nb")]);
    control.body = Some(HttpBody::Text("body".to_owned()));
    let mut forbidden = get(format!("http://{addr}/"));
    forbidden.headers = pairs(&[("Connection", "close")]);
    forbidden.body = Some(HttpBody::Text("body".to_owned()));

    let executor = executor(HttpVersion::Auto);
    for request in [ftp, control, forbidden] {
        let hop = send(&executor, &request).await;
        assert_eq!(
            failure(&hop),
            (FailureKind::InvalidRequest, FailureStage::Prepare),
            "{request:?}"
        );
        assert_eq!(hop.sizes.sent, 0);
        assert_eq!(hop.connection, None);
    }
    assert_eq!(listener.accepted().await, 0);
}

fn with_distinct_headers(url: String, count: usize) -> HttpRequest {
    let mut request = get(url);
    request.headers = (0..count).map(|n| (format!("x-{n}"), "v".into())).collect();
    request
}

/// A request holds at most 4,096 header names of its own.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_with_too_many_headers_fails_in_prepare() {
    let most = 4096;
    let listener = CountingListener::bind().await;
    let request = with_distinct_headers(format!("http://{}/", listener.addr), most + 1);
    let hop = send(&executor(HttpVersion::Auto), &request).await;
    assert_eq!(
        failure(&hop),
        (FailureKind::InvalidRequest, FailureStage::Prepare)
    );
    assert_eq!(hop.connection, None);
    assert_eq!(listener.accepted().await, 0);

    // The most a request can hold reaches hyper, which adds its own headers
    // without a panic. The server refuses so many: HTTP/1.1 answers `431`,
    // or resets while the headers are still being written, and HTTP/2
    // resets the stream.
    let addr = spawn_auto(echo).await;
    for version in [HttpVersion::Http1, HttpVersion::Http2] {
        let request = with_distinct_headers(format!("http://{addr}/"), most);
        let hop = send(&executor(version), &request).await;
        assert!(hop.connection.is_some(), "{version:?}");
        match &hop.outcome {
            Ok(response) => assert_eq!(response.status, 431, "{version:?}"),
            Err(error) => assert_eq!(error.kind, FailureKind::ConnectionClosed, "{version:?}"),
        }
    }
}

/// `prepare` can't be cut short, so its time is charged once it is done.
/// Copying a 4 MiB body takes far longer than the microsecond allowed.
#[tokio::test(flavor = "multi_thread")]
async fn a_prepare_that_uses_up_the_timeout_fails_in_prepare() {
    let listener = CountingListener::bind().await;
    let mut request = get(format!("http://{}/", listener.addr));
    request.body = Some(HttpBody::Text("x".repeat(4 * MIB as usize)));
    let executor = executor(HttpVersion::Auto);
    let hop = send_within(&executor, &request, Duration::from_micros(1)).await;
    assert_eq!(failure(&hop), (FailureKind::Timeout, FailureStage::Prepare));
    assert_eq!(hop.sizes.sent, 0);
    assert_eq!(hop.connection, None);

    // A request that fails its checks says so, however long they took.
    let ftp = get(format!("ftp://{}/", listener.addr));
    let hop = send_within(&executor, &ftp, Duration::from_micros(1)).await;
    assert_eq!(
        failure(&hop),
        (FailureKind::InvalidRequest, FailureStage::Prepare)
    );
    assert_eq!(listener.accepted().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_zero_timeout_fails_in_prepare() {
    let listener = CountingListener::bind().await;
    let hop = send_within(
        &executor(HttpVersion::Auto),
        &get(format!("http://{}/", listener.addr)),
        Duration::ZERO,
    )
    .await;
    assert_eq!(failure(&hop), (FailureKind::Timeout, FailureStage::Prepare));
    assert_eq!(hop.connection, None);
    assert_eq!(listener.accepted().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn failures_never_show_the_url() {
    let refused = refused_port();
    let ca = TestCa::generate();
    let untrusted = spawn_tls(echo, &[b"http/1.1"], None, ca.leaf_for_ip(LOCALHOST)).await;
    let executor = trusting(&TestCa::generate(), HttpVersion::Auto);
    for (addr, kind) in [
        (refused.addr, FailureKind::Connect),
        (untrusted, FailureKind::Tls),
    ] {
        let url = format!("https://{addr}/secret-path?token=hunter2");
        let hop = send(&executor, &get(url)).await;
        assert_eq!(failure(&hop), (kind, FailureStage::Connect));
        let error = hop.outcome.as_ref().unwrap_err();
        let shown = format!("{error} {error:?} {hop:?}");
        for secret in ["hunter2", "secret-path", "127.0.0.1"] {
            assert!(!shown.contains(secret), "{shown}");
        }
    }
}
