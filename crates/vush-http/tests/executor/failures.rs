//! Hops that fail, each from a real cause, with the exact kind and stage.
//! None of them may land on `other`.

use std::sync::Arc;
use std::time::Duration;

use rustls::RootCertStore;
use vush_core::plan::HttpVersion;
use vush_core::protocol::http::HttpBody;
use vush_core::protocol::{Failure, FailureKind, FailureStage};
use vush_http::{BuildError, Executor};

use crate::common::{
    CountingListener, LOCALHOST, MIB, TestCa, close_after_head, echo, executor, failure,
    garbage_status, get, h2_reset_after_one_frame, hold, malformed_chunk, pairs, refused_port,
    send, send_within, settings, silent_after_head, spawn_raw, spawn_tls, stalled_body, trusting,
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

#[tokio::test(flavor = "multi_thread")]
async fn unsupported_tls_settings_fail_to_build() {
    let mut ca_file = settings(HttpVersion::Auto, MIB);
    ca_file.tls.ca_file = Some("ca.pem".to_owned());
    let mut insecure = settings(HttpVersion::Auto, MIB);
    insecure.tls.insecure = true;
    for (settings, key) in [(ca_file, "tls.ca_file"), (insecure, "tls.insecure")] {
        for built in [
            Executor::new(&settings),
            Executor::with_roots(&settings, RootCertStore::empty()),
        ] {
            let error = built.err().expect("the setting isn't supported yet");
            assert!(
                matches!(error, BuildError::Unsupported(named) if named == key),
                "{error:?}"
            );
            assert_eq!(error.to_string(), format!("`{key}` isn't supported yet"));
        }
    }
}
