//! Hops that fail, each from a real cause, with the exact kind and stage.
//! None of them may land on `other`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use vush_core::plan::{HttpSettings, HttpVersion, MAX_REQUEST_HEADERS, ResourceLimits};
use vush_core::protocol::http::{HttpBody, HttpRequest};
use vush_core::protocol::{Failure, FailureKind, FailureStage};
use vush_http::Executor;

use crate::common::{
    CountingListener, HEAD_BUFFER_BYTES, LOCALHOST, MIB, NO_DIR, TestCa, close_after_head, echo,
    executor, failure, garbage_status, get, h2_reset_after_one_frame, head_of_1_mb, head_of_len,
    head_of_the_buffer, hold, http10_to_close, hundred_and_one_headers, hundred_headers, limited,
    malformed_chunk, pairs, poll_once, refused_port, reply, send, send_within, silent_after_head,
    spawn_auto, spawn_raw, spawn_tls, stalled_body, trusting, two_mib_promised_then_stall,
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

/// The lone executor's connect limit is `settings.timeout`, which is shorter
/// than the hop's time here, so the connect's own limit ends the hop.
#[tokio::test(flavor = "multi_thread")]
async fn a_lone_executor_bounds_connects_by_its_timeout() {
    let listener = CountingListener::bind().await;
    let settings = HttpSettings {
        timeout: Duration::from_millis(200),
        ..HttpSettings::default()
    };
    let executor = Executor::with_roots(
        &settings,
        &ResourceLimits::default(),
        Path::new(NO_DIR),
        TestCa::generate().roots(),
    )
    .unwrap();
    // The kernel completes the TCP connect and nobody answers the handshake.
    let hop = send_within(
        &executor,
        &get(format!("https://{}/", listener.addr)),
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(failure(&hop), (FailureKind::Timeout, FailureStage::Connect));
    assert!(
        hop.timing.total < Duration::from_secs(5),
        "{:?}",
        hop.timing
    );
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
/// every hop waiting for it. Each of them fails as `tls`, never `other`: a
/// waiting hop gets the kind through its pool's record, and a hop that
/// started after the failure through its own connect.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_shared_http2_connect_fails_every_waiter_as_tls() {
    let leaf = TestCa::generate().leaf_for_ip(LOCALHOST);
    let addr = spawn_tls(echo, &[b"h2"], None, leaf).await;
    let executor = Arc::new(trusting(&TestCa::generate(), HttpVersion::Http2));
    let hops = [(); 6].map(|()| {
        let executor = executor.clone();
        let request = get(format!("https://{addr}/"));
        tokio::spawn(async move { send(&executor, &request).await })
    });
    let tls = Failure {
        kind: FailureKind::Tls,
        stage: FailureStage::Connect,
        message: "the TLS handshake failed",
    };
    for hop in hops {
        let hop = hop.await.unwrap();
        assert_eq!(hop.connection, None);
        assert_eq!(*hop.outcome.as_ref().unwrap_err(), tls);
    }
}

/// A hop that waits on a shared connect that is then abandoned has no failure
/// to report, so it keeps `connect`, even though an earlier connect failed
/// as `tls`.
#[tokio::test(flavor = "multi_thread")]
async fn an_abandoned_shared_http2_connect_leaves_its_waiter_with_connect() {
    let leaf = TestCa::generate().leaf_for_ip(LOCALHOST);
    // The server answers one handshake, so the second connect's is held.
    let addr = spawn_tls(echo, &[b"h2"], Some(1), leaf).await;
    let executor = trusting(&TestCa::generate(), HttpVersion::Http2);
    let url = format!("https://{addr}/");
    let first = send(&executor, &get(url.clone())).await;
    assert_eq!(failure(&first), (FailureKind::Tls, FailureStage::Connect));

    let request = get(url);
    let mut connecting = Box::pin(send(&executor, &request));
    assert!(poll_once(&mut connecting).await.is_pending());
    let mut waiting = Box::pin(send(&executor, &request));
    assert!(poll_once(&mut waiting).await.is_pending());
    drop(connecting);

    let hop = waiting.await;
    assert_eq!(hop.connection, None);
    assert_eq!(
        *hop.outcome.as_ref().unwrap_err(),
        Failure {
            kind: FailureKind::Connect,
            stage: FailureStage::Connect,
            message: "the shared connection attempt failed",
        }
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
async fn a_stall_past_the_limit_times_out_in_body() {
    let addr = spawn_raw(two_mib_promised_then_stall).await;
    let hop = send_within(
        &limited(HttpVersion::Auto, 1024),
        &get(format!("http://{addr}/")),
        Duration::from_millis(500),
    )
    .await;
    assert_eq!(failure(&hop), (FailureKind::Timeout, FailureStage::Body));
    // More than the kept bytes: the body counts what was read, not what
    // was kept.
    assert!(hop.sizes.received > 1024, "{}", hop.sizes.received);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_http10_body_past_the_limit_reads_to_close() {
    let addr = spawn_raw(http10_to_close).await;
    let hop = send(
        &limited(HttpVersion::Auto, 1024),
        &get(format!("http://{addr}/")),
    )
    .await;
    let got = crate::common::response(&hop);
    assert_eq!(got.body.len(), 1024);
    assert!(got.truncated);
    // All 64 writes of 4096 are read, up to the close.
    assert_eq!(hop.sizes.received, 64 * 4096);
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

/// A request holds at most `MAX_REQUEST_HEADERS` header names of its own.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_with_too_many_headers_fails_in_prepare() {
    let most = MAX_REQUEST_HEADERS;
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
    // without a panic. HTTP/1.1 servers refuse more than 100 headers: the
    // answer is `431`, or a reset while the headers are still being written.
    // The HTTP/2 server takes them, since they fit its header list.
    let addr = spawn_auto(echo).await;
    let request = with_distinct_headers(format!("http://{addr}/"), most);
    let hop = send(&executor(HttpVersion::Http1), &request).await;
    assert!(hop.connection.is_some());
    match &hop.outcome {
        Ok(response) => assert_eq!(response.status, 431),
        Err(error) => assert_eq!(error.kind, FailureKind::ConnectionClosed),
    }
    let hop = send(&executor(HttpVersion::Http2), &request).await;
    assert!(hop.connection.is_some());
    match &hop.outcome {
        Ok(response) => assert_eq!(response.status, 200),
        Err(error) => panic!("HTTP/2 should accept {most} headers: {error:?}"),
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

/// hyper's default of 100 response headers is pinned here: the 101st makes
/// the head too large.
#[tokio::test(flavor = "multi_thread")]
async fn a_head_of_100_headers_passes_and_101_is_too_large() {
    let executor = executor(HttpVersion::Http1);
    let addr = spawn_raw(hundred_headers).await;
    let hop = send(&executor, &get(format!("http://{addr}/"))).await;
    let Ok(response) = &hop.outcome else {
        panic!("100 headers should pass: {:?}", hop.outcome)
    };
    assert_eq!(response.status, 200);

    let addr = spawn_raw(hundred_and_one_headers).await;
    let hop = send(&executor, &get(format!("http://{addr}/"))).await;
    assert_eq!(
        failure(&hop),
        (FailureKind::Protocol, FailureStage::Headers)
    );
    let Err(failed) = &hop.outcome else {
        unreachable!()
    };
    assert_eq!(failed.message, "the response head is too large");
}

/// The head buffer is 417,792 bytes, 408 KiB. hyper parses what it has
/// before it checks the size, and a head is too large only when it is still
/// incomplete with at least that much buffered. So a head of up to 417,792
/// bytes always passes, within the 100-header cap. One read fills all of the
/// buffer's spare capacity, which can be more than the limit leaves, so a
/// longer head can pass when one read carries it past the limit: measured
/// here, 600 KB passed, and 840 KB and 1 MB failed. Memory stays bounded by
/// hyper's read buffer either way.
#[tokio::test(flavor = "multi_thread")]
async fn a_head_that_fills_the_buffer_passes_and_1_mb_is_too_large() {
    let head = head_of_len(HEAD_BUFFER_BYTES);
    assert_eq!(head.len(), 417_792);
    assert!(head.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert!(head.ends_with(b"\r\ncontent-length: 0\r\n\r\n"));
    let executor = executor(HttpVersion::Http1);
    let addr = spawn_raw(head_of_the_buffer).await;
    let hop = send(&executor, &get(format!("http://{addr}/"))).await;
    let Ok(response) = &hop.outcome else {
        panic!(
            "a head that fills the buffer should pass: {:?}",
            hop.outcome
        )
    };
    assert_eq!(response.status, 200);

    let addr = spawn_raw(head_of_1_mb).await;
    let hop = send(&executor, &get(format!("http://{addr}/"))).await;
    assert_eq!(
        failure(&hop),
        (FailureKind::Protocol, FailureStage::Headers)
    );
    let Err(failed) = &hop.outcome else {
        unreachable!()
    };
    assert_eq!(failed.message, "the response head is too large");
}

/// The client advertises a 16 KiB header list, so a larger head is a
/// protocol failure rather than a reset or a hang.
#[tokio::test(flavor = "multi_thread")]
async fn an_http2_head_over_16_kib_is_a_protocol_failure() {
    let addr =
        spawn_auto(|_request| async { reply(200, &[("x-big", &"a".repeat(17_000))], Vec::new()) })
            .await;
    let hop = send(
        &executor(HttpVersion::Http2),
        &get(format!("http://{addr}/")),
    )
    .await;
    assert_eq!(
        failure(&hop),
        (FailureKind::Protocol, FailureStage::Headers)
    );
}
