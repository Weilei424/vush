//! The `HttpExecutor` contract's eight points against the real executor, in
//! order. A point already covered elsewhere has a pointer to its tests.

use std::path::Path;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use rustls::RootCertStore;
use vush_core::plan::{HttpSettings, HttpVersion, PoolScope, ResourceLimits};
use vush_core::protocol::http::{HttpBody, HttpExecutor};
use vush_core::protocol::{FailureKind, FailureStage};
use vush_http::Executor;

use crate::common::{
    BODY_DELAY, CountingServer, DropAt, Dropper, HEAD_DELAY, LOCALHOST, MIB, NO_DIR, TIMEOUT,
    TestCa, after_two_arrivals, early_hints_then_ok, echo, executor, failure, get, method,
    poll_once, response, send, send_within, settings, silent_after_head, slow_reply, spawn_h1,
    spawn_h1_dropper, spawn_h2_dropper, spawn_raw, spawn_tls, trusting,
};

// ---- Point 1: one hop per call ----
//
// See also `invalid_requests_send_nothing` in `failures.rs`.

#[tokio::test(flavor = "multi_thread")]
async fn one_send_reaches_the_server_once() {
    let server = CountingServer::spawn().await;
    let executor = executor(HttpVersion::Http1);

    let moved = send(&executor, &get(format!("http://{}/moved", server.addr))).await;
    assert_eq!(response(&moved).status, 302);
    assert_eq!(server.hits("/moved"), 1);
    assert_eq!(server.hits("/target"), 0);

    let busy = send(&executor, &get(format!("http://{}/busy", server.addr))).await;
    assert_eq!(response(&busy).status, 503);
    assert_eq!(server.hits("/busy"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_informational_response_is_not_the_hops_response() {
    let addr = spawn_raw(early_hints_then_ok).await;
    let hop = send(
        &executor(HttpVersion::Http1),
        &get(format!("http://{addr}/")),
    )
    .await;
    let got = response(&hop);
    assert_eq!(got.status, 200);
    assert_eq!(got.body, b"ok");
}

// ---- Point 2: finite time ----
//
// See also the stage timeouts in `failures.rs`.

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_response_still_succeeds_and_times_its_parts() {
    let addr = spawn_raw(slow_reply).await;
    let hop = send(
        &executor(HttpVersion::Http1),
        &get(format!("http://{addr}/")),
    )
    .await;
    assert_eq!(response(&hop).body, b"helloworld");
    let to_headers = hop.timing.to_headers.expect("the head arrived");
    assert!(to_headers >= HEAD_DELAY, "{to_headers:?}");
    assert!(
        hop.timing.total >= HEAD_DELAY + BODY_DELAY,
        "{:?}",
        hop.timing
    );
    assert!(hop.timing.total >= to_headers);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_upload_times_out_in_headers() {
    let addr = spawn_raw(silent_after_head).await;
    let mut request = get(format!("http://{addr}/"));
    request.method = method("POST");
    request.body = Some(HttpBody::Text("x".repeat(32 << 20)));
    let timeout = Duration::from_millis(500);

    let hop = send_within(&executor(HttpVersion::Http1), &request, timeout).await;

    assert_eq!(failure(&hop), (FailureKind::Timeout, FailureStage::Headers));
    assert_eq!(hop.timing.to_headers, None);
    assert!(hop.timing.total >= timeout, "{:?}", hop.timing);
}

// ---- Point 3: no panics ----
//
// See `invalid_requests_send_nothing` and
// `a_request_with_too_many_headers_fails_in_prepare` in `failures.rs`.

// ---- Point 4: cancel by drop ----

/// Sends one hop on `session` and drops it once `dropper` says it reached
/// its drop point. The hop must not finish first.
async fn drop_hop_when_reached(
    executor: &Executor,
    session: &mut vush_http::Session,
    url: &str,
    dropper: &mut Dropper,
) {
    let request = get(url.to_owned());
    let hop = executor.send(session, &request, TIMEOUT);
    tokio::select! {
        _ = hop => panic!("the hop finished before the server held it"),
        () = dropper.reached() => {}
    }
}

/// A lone HTTP/1.1 executor with the pool scope `pool`.
fn lone_http1(pool: PoolScope) -> Executor {
    let settings = HttpSettings {
        pool,
        ..settings(HttpVersion::Http1, MIB)
    };
    Executor::with_roots(
        &settings,
        &ResourceLimits::default(),
        Path::new(NO_DIR),
        RootCertStore::empty(),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_an_http1_hop_closes_its_connection() {
    for at in [DropAt::BeforeHead, DropAt::InBody] {
        for pool in [PoolScope::Shared, PoolScope::Vu] {
            let label = format!("{at:?} under {pool:?}");
            let mut dropper = spawn_h1_dropper(at).await;
            let url = format!("http://{}/", dropper.addr);
            let executor = lone_http1(pool);
            let mut session = executor.new_session();

            drop_hop_when_reached(&executor, &mut session, &url, &mut dropper).await;

            tokio::time::timeout(TIMEOUT, dropper.closed())
                .await
                .unwrap_or_else(|_| panic!("the connection stayed open: {label}"));
            let next = executor.send(&mut session, &get(url), TIMEOUT).await;
            assert_eq!(response(&next).status, 200, "{label}");
            assert!(next.timing.setup.is_some(), "{label}");
            assert_eq!(dropper.accepted(), 2, "{label}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_an_http2_hop_resets_only_its_stream() {
    for at in [DropAt::BeforeHead, DropAt::InBody] {
        let ca = TestCa::generate();
        let mut dropper = spawn_h2_dropper(at, &ca).await;
        let url = format!("https://{}/", dropper.addr);
        let executor = trusting(&ca, HttpVersion::Http2);
        let mut session = executor.new_session();

        drop_hop_when_reached(&executor, &mut session, &url, &mut dropper).await;

        // The server answers one handshake, so a second connection can't
        // serve this hop: it ran on the same connection.
        let next = executor.send(&mut session, &get(url), TIMEOUT).await;
        assert_eq!(response(&next).status, 200, "{at:?}");
        // A dropped hop reports nothing, so the setup of the connection it
        // opened goes to the next hop that uses it.
        assert!(next.timing.setup.is_some(), "{at:?}");
        assert_eq!(dropper.accepted(), 2, "{at:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_a_waiting_http2_hop_leaves_the_shared_connect_alone() {
    let ca = TestCa::generate();
    let addr = spawn_tls(echo, &[b"h2"], Some(1), ca.leaf_for_ip(LOCALHOST)).await;
    let request = get(format!("https://{addr}/"));
    let executor = trusting(&ca, HttpVersion::Http2);
    let mut first = executor.new_session();
    let mut second = executor.new_session();

    let mut connecting = Box::pin(executor.send(&mut first, &request, TIMEOUT));
    assert!(matches!(poll_once(&mut connecting).await, Poll::Pending));
    let mut waiting = Box::pin(executor.send(&mut second, &request, TIMEOUT));
    assert!(matches!(poll_once(&mut waiting).await, Poll::Pending));
    drop(waiting);

    let connected = connecting.await;
    response(&connected);
    assert!(connected.timing.setup.is_some());
    let third = send(&executor, &request).await;
    response(&third);
    assert_eq!(third.connection, connected.connection);
    assert!(third.timing.setup.is_none());
}

// ---- Points 5 to 8 ----
//
// 5, a session's cookies: `cookies.rs`; `a_copy_keeps_its_own_cookies` and
// `a_session_from_another_executor_is_refused` for copies and foreign
// sessions.
// 6, redirects: `follow`'s unit tests and
// `the_executor_follows_with_the_redirect_rules` in `send.rs`.
// 7 and 8, what a failure may show: `diagnostics.rs`.

// ---- Timing and reuse ----
//
// See also `connections_are_reused_without_a_race` in `send.rs`.

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_http1_hops_open_a_connection_each() {
    let addr = spawn_h1(after_two_arrivals()).await;
    let request = get(format!("http://{addr}/"));
    let executor = Arc::new(executor(HttpVersion::Http1));

    let hops = both_at_once(&executor, &request).await;

    for hop in &hops {
        response(hop);
        let setup = hop.timing.setup.expect("a new connection");
        assert!(setup.tls.is_none());
    }
    assert_ne!(hops[0].connection, hops[1].connection);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_http2_hops_share_one_connection() {
    let ca = TestCa::generate();
    let addr = spawn_tls(
        after_two_arrivals(),
        &[b"h2"],
        Some(1),
        ca.leaf_for_ip(LOCALHOST),
    )
    .await;
    let request = get(format!("https://{addr}/"));
    let executor = Arc::new(trusting(&ca, HttpVersion::Http2));

    let hops = both_at_once(&executor, &request).await;

    for hop in &hops {
        response(hop);
    }
    assert!(hops[0].connection.is_some());
    assert_eq!(hops[0].connection, hops[1].connection);
    let setups = hops.iter().filter(|hop| hop.timing.setup.is_some()).count();
    assert_eq!(setups, 1);
}

/// Sends `request` twice at once, each on a session of its own.
async fn both_at_once(
    executor: &Arc<Executor>,
    request: &vush_core::protocol::http::HttpRequest,
) -> [vush_core::protocol::http::HttpHop; 2] {
    let start = |executor: Arc<Executor>, request| {
        tokio::spawn(async move {
            let mut session = executor.new_session();
            executor.send(&mut session, &request, TIMEOUT).await
        })
    };
    let one = start(executor.clone(), request.clone());
    let two = start(executor.clone(), request.clone());
    [one.await.unwrap(), two.await.unwrap()]
}
