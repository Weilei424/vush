//! The `HttpExecutor` contract's eight points against the real executor, in
//! order. A point already covered elsewhere has a pointer to its tests.

use std::path::Path;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use rustls::RootCertStore;
use vush_core::plan::{HttpSettings, HttpVersion, PoolScope, ResourceLimits};
use vush_core::protocol::http::{HttpBody, HttpExecutor, HttpHop};
use vush_core::protocol::{Failure, FailureKind, FailureStage, Outcome};
use vush_core::runner::run_attempt;
use vush_http::Executor;

use crate::common::{
    BODY_DELAY, CountingServer, DropAt, Dropper, HEAD_DELAY, LOCALHOST, MIB, NO_DIR, TIMEOUT,
    TestCa, after_two_arrivals, bad_content_length, both_framings, close_after_head,
    early_hints_then_ok, echo, executor, failure, garbage_status, get, h2_data_on_stream_zero,
    h2_goaway_before_response, http_reply_to_tls, method, pairs, poll_once, redirecting,
    refused_port, response, send, send_within, settings, short_body_then_close, silent_after_head,
    slow_reply, spawn_h1, spawn_h1_dropper, spawn_h2_dropper, spawn_h2_mover, spawn_raw, spawn_tls,
    spawn_upload_staller, trusting, two_content_lengths,
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
    let mut staller = spawn_upload_staller().await;
    let mut request = get(format!("http://{}/", staller.addr));
    request.method = method("POST");
    request.body = Some(HttpBody::Text("x".repeat(32 << 20)));
    let timeout = Duration::from_millis(500);

    let hop = send_within(&executor(HttpVersion::Http1), &request, timeout).await;

    assert_eq!(failure(&hop), (FailureKind::Timeout, FailureStage::Headers));
    assert_eq!(hop.timing.to_headers, None);
    assert!(hop.timing.total >= timeout, "{:?}", hop.timing);
    // The server had read part of the declared body, and had stopped
    // reading, when the hop ended, so the body was being sent. Loopback
    // socket buffers hold far less than the other 31 MiB, so the upload was
    // still incomplete.
    let stopped = staller
        .stopped()
        .expect("the server read part of the body first");
    assert_eq!(stopped.declared, 32 << 20);
    assert!(stopped.read >= MIB, "{}", stopped.read);
}

// ---- Point 3: no panics ----
//
// See `invalid_requests_send_nothing` and
// `a_request_with_too_many_headers_fails_in_prepare` in `failures.rs`.

/// One catalogue row: checks that the cause gave `kind` with this stage and
/// text, and records `kind` as covered.
fn row(
    covered: &mut Vec<FailureKind>,
    kind: FailureKind,
    hop_failure: &Failure,
    stage: FailureStage,
    message: &'static str,
) {
    assert_eq!(hop_failure.kind, kind, "{kind:?}");
    assert_eq!(
        hop_failure,
        &Failure {
            kind,
            stage,
            message
        },
        "{kind:?}"
    );
    covered.push(kind);
}

fn failed(hop: &HttpHop) -> &Failure {
    hop.outcome.as_ref().expect_err("the hop should fail")
}

#[tokio::test(flavor = "multi_thread")]
async fn every_failure_kind_has_a_real_cause() {
    let mut covered = Vec::new();

    // `invalid_request`: a header value with a control character reaches
    // no server.
    let server = CountingServer::spawn().await;
    let mut bad = get(format!("http://{}/probe", server.addr));
    bad.headers = pairs(&[("x-bad", "a\r\nb")]);
    let hop = send(&executor(HttpVersion::Auto), &bad).await;
    row(
        &mut covered,
        FailureKind::InvalidRequest,
        failed(&hop),
        FailureStage::Prepare,
        "a header value holds a control character",
    );
    assert_eq!(server.hits("/probe"), 0);

    // `connect`: nothing listens on the port.
    let refused = refused_port();
    let hop = send(
        &executor(HttpVersion::Auto),
        &get(format!("http://{}/", refused.addr)),
    )
    .await;
    row(
        &mut covered,
        FailureKind::Connect,
        failed(&hop),
        FailureStage::Connect,
        "the connection was refused or unreachable",
    );

    // `tls`: the server's certificate is from a CA the client doesn't trust.
    let addr = spawn_tls(
        echo,
        &[b"h2", b"http/1.1"],
        None,
        TestCa::generate().leaf_for_ip(LOCALHOST),
    )
    .await;
    let hop = send(
        &trusting(&TestCa::generate(), HttpVersion::Auto),
        &get(format!("https://{addr}/")),
    )
    .await;
    row(
        &mut covered,
        FailureKind::Tls,
        failed(&hop),
        FailureStage::Connect,
        "the TLS handshake failed",
    );

    // `timeout`: the server reads the request and says nothing.
    let addr = spawn_raw(silent_after_head).await;
    let hop = send_within(
        &executor(HttpVersion::Auto),
        &get(format!("http://{addr}/")),
        Duration::from_millis(300),
    )
    .await;
    row(
        &mut covered,
        FailureKind::Timeout,
        failed(&hop),
        FailureStage::Headers,
        "the hop ran out of time",
    );

    // `connection_closed`: the server closes after the request head.
    let addr = spawn_raw(close_after_head).await;
    let hop = send(
        &executor(HttpVersion::Auto),
        &get(format!("http://{addr}/")),
    )
    .await;
    row(
        &mut covered,
        FailureKind::ConnectionClosed,
        failed(&hop),
        FailureStage::Headers,
        "the connection closed before the response was complete",
    );

    // `protocol`: the status line isn't HTTP.
    let addr = spawn_raw(garbage_status).await;
    let hop = send(
        &executor(HttpVersion::Auto),
        &get(format!("http://{addr}/")),
    )
    .await;
    row(
        &mut covered,
        FailureKind::Protocol,
        failed(&hop),
        FailureStage::Headers,
        "the response broke HTTP rules",
    );

    // `invalid_redirect`: a `302` to an `ftp` URL.
    let addr = spawn_h1(redirecting(&[("/", 302, "ftp://h/")])).await;
    let executor = executor(HttpVersion::Auto);
    let hop = send(&executor, &get(format!("http://{addr}/"))).await;
    let moved = response(&hop);
    assert_eq!(moved.status, 302);
    let error = executor
        .follow(&get("http://127.0.0.1/".to_owned()), moved)
        .expect_err("an ftp redirect can't be followed");
    row(
        &mut covered,
        FailureKind::InvalidRedirect,
        &error,
        FailureStage::Prepare,
        "the redirect goes to an unsupported scheme",
    );

    // `too_many_redirects`: a path that redirects to itself, with 2 allowed.
    let addr = spawn_h1(redirecting(&[("/loop", 302, "/loop")])).await;
    let mut session = executor.new_session();
    let result = run_attempt(
        &executor,
        &mut session,
        get(format!("http://{addr}/loop")),
        TIMEOUT,
        2,
    )
    .await;
    assert_eq!(
        result.record.outcome(),
        Outcome::Failed {
            kind: FailureKind::TooManyRedirects,
            stage: FailureStage::Prepare,
        }
    );
    covered.push(FailureKind::TooManyRedirects);

    // `dns` has no row. Its cause needs the private resolver hook, so
    // `a_failed_lookup_fails_the_hop_as_dns` in `src/executor.rs` takes it
    // through `send`. `other` is a kind no cause should reach.
    assert!(!covered.contains(&FailureKind::Other));
    for kind in FailureKind::ALL {
        if kind != FailureKind::Dns && kind != FailureKind::Other {
            assert!(covered.contains(&kind), "no row for {kind:?}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn hostile_responses_get_a_kind_that_fits() {
    const PROTOCOL: (FailureKind, FailureStage, &str) = (
        FailureKind::Protocol,
        FailureStage::Headers,
        "the response broke HTTP rules",
    );
    const CLOSED: (FailureKind, FailureStage, &str) = (
        FailureKind::ConnectionClosed,
        FailureStage::Headers,
        "the connection closed before the response was complete",
    );
    let plain = executor(HttpVersion::Auto);
    let h2 = executor(HttpVersion::Http2);

    let mut cases = Vec::new();
    for (name, fixture, executor, expected) in [
        (
            "two content-lengths",
            two_content_lengths as fn(_) -> _,
            &plain,
            PROTOCOL,
        ),
        ("a bad content-length", bad_content_length, &plain, PROTOCOL),
        (
            "a short body",
            short_body_then_close,
            &plain,
            (FailureKind::ConnectionClosed, FailureStage::Body, CLOSED.2),
        ),
        ("a goaway", h2_goaway_before_response, &h2, CLOSED),
        ("data on stream zero", h2_data_on_stream_zero, &h2, PROTOCOL),
    ] {
        let addr = spawn_raw(fixture).await;
        let hop = send(executor, &get(format!("http://{addr}/"))).await;
        cases.push((name, hop, expected));
    }

    let ca = TestCa::generate();
    let addr = spawn_raw(http_reply_to_tls).await;
    let hop = send(
        &trusting(&ca, HttpVersion::Auto),
        &get(format!("https://{addr}/")),
    )
    .await;
    cases.push((
        "an http reply to tls",
        hop,
        (
            FailureKind::Tls,
            FailureStage::Connect,
            "the TLS handshake failed",
        ),
    ));

    for (name, hop, (kind, stage, message)) in &cases {
        let got = failed(hop);
        assert_ne!(got.kind, FailureKind::Other, "{name}");
        assert_eq!(
            got,
            &Failure {
                kind: *kind,
                stage: *stage,
                message
            },
            "{name}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_response_with_both_framings_is_read_as_chunked() {
    // RFC 9112 section 6.3: `Transfer-Encoding` wins over `Content-Length`.
    let addr = spawn_raw(both_framings).await;
    let hop = send(
        &executor(HttpVersion::Auto),
        &get(format!("http://{addr}/")),
    )
    .await;
    let got = response(&hop);
    assert_eq!(got.status, 200);
    assert_eq!(got.body, b"ok");
}

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
    // Biased: a hop that has finished always wins, so it fails the test
    // even when the dropper's signal is ready too.
    tokio::select! {
        biased;
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

        // hyper's server drops the first request's handler, or its body, when
        // the client resets the stream. The connection stays open below, so
        // nothing else dropped it.
        tokio::time::timeout(TIMEOUT, dropper.reset())
            .await
            .unwrap_or_else(|_| panic!("the stream wasn't reset: {at:?}"));

        // The server answers one handshake, so a second connection can't
        // serve this hop: it ran on the same connection.
        let next = executor.send(&mut session, &get(url), TIMEOUT).await;
        assert_eq!(response(&next).status, 200, "{at:?}");
        // The dropped hop took the setup of the connection it opened when it
        // got the connection, so the next hop that uses it has none.
        assert!(next.timing.setup.is_none(), "{at:?}");
        assert_eq!(dropper.accepted(), 1, "{at:?}");
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

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_hop_takes_the_setup_of_the_connection_it_was_moved_to() {
    let mut mover = spawn_h2_mover().await;
    let request = get(format!("http://{}/", mover.addr));
    let executor = executor(HttpVersion::Http2);

    // The server's SETTINGS, one stream at a time, came before this response.
    let warm = send(&executor, &request).await;
    response(&warm);
    assert!(warm.timing.setup.is_some());

    // `holding` takes the connection's one stream. `pending` waits in h2 for
    // it, which stops the connection taking more, so `moved` waits in
    // hyper's queue. Cutting the connection hands `moved` back, and
    // hyper-util sends it on a new connection.
    let (mut one, mut two, mut three) = (
        executor.new_session(),
        executor.new_session(),
        executor.new_session(),
    );
    let mut holding = Box::pin(executor.send(&mut one, &request, TIMEOUT));
    tokio::select! {
        biased;
        _ = &mut holding => panic!("the first hop finished before the server held it"),
        () = mover.held() => {}
    }
    let mut pending = Box::pin(executor.send(&mut two, &request, TIMEOUT));
    assert!(matches!(poll_once(&mut pending).await, Poll::Pending));
    let mut moved = Box::pin(executor.send(&mut three, &request, TIMEOUT));
    assert!(matches!(poll_once(&mut moved).await, Poll::Pending));
    mover.cut();
    tokio::select! {
        biased;
        _ = &mut moved => panic!("the moved hop finished before the server held it"),
        () = mover.held() => {}
    }
    assert_eq!(mover.accepted(), 2);
    drop((moved, pending, holding));

    // The dropped hop took the new connection's setup with it.
    let next = send(&executor, &request).await;
    response(&next);
    assert_eq!(mover.accepted(), 2);
    assert!(next.timing.setup.is_none());
}

// ---- Point 5: no per-user state in the executor ----
//
// See `cookies.rs`.

// ---- Point 6: sessions ----
//
// See `a_copy_keeps_its_own_cookies` in `cookies.rs` and
// `a_session_from_another_executor_is_refused` in `pools.rs`.

// ---- Point 7: `follow` is pure ----
//
// See `follow`'s unit tests and `the_executor_follows_with_the_redirect_rules`
// in `send.rs`.

// ---- Point 8: safe text ----
//
// See `diagnostics.rs`.

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
