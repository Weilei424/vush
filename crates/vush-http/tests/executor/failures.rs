//! Hops that fail, each from a real cause, with the exact kind and stage.
//! None of them may land on `other`.

use std::time::Duration;

use rustls::RootCertStore;
use vush_core::plan::HttpVersion;
use vush_core::protocol::http::HttpBody;
use vush_core::protocol::{FailureKind, FailureStage};
use vush_http::{BuildError, Executor};

use crate::common::{
    CountingListener, LOCALHOST, MIB, TestCa, close_after_head, echo, executor, failure,
    garbage_status, get, h2_reset_after_one_frame, hold, pairs, refused_addr, send, send_within,
    settings, silent_after_head, spawn_raw, spawn_tls, stalled_body, trusting,
};

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_port_is_connect() {
    let addr = refused_addr().await;
    let hop = send(
        &executor(HttpVersion::Auto),
        &get(format!("http://{addr}/")),
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
    let refused = refused_addr().await;
    let ca = TestCa::generate();
    let untrusted = spawn_tls(echo, &[b"http/1.1"], None, ca.leaf_for_ip(LOCALHOST)).await;
    let executor = trusting(&TestCa::generate(), HttpVersion::Auto);
    for (addr, kind) in [
        (refused, FailureKind::Connect),
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
