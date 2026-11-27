//! The HTTP/2 stream gate under `version: http2`, and under `auto` when ALPN
//! picks HTTP/2. A hop waits in Vush's own queue, in `connect`, until its
//! connection has a free stream, so a hop that ends there never sends its
//! request. Hops take streams in the order they asked, up to the server's
//! limit as it changes. A `GOAWAY` moves the hops still waiting, and a dead
//! connection moves them or fails them in `connect`. Under `auto`, an origin
//! keeps one HTTP/2 connection, however many hops' connects ALPN makes
//! HTTP/2.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::task::JoinHandle;
use vush_core::plan::HttpVersion;
use vush_core::protocol::http::{HttpBody, HttpHop, HttpRequest};
use vush_core::protocol::{FailureKind, FailureStage};
use vush_http::Executor;

use crate::common::{
    LOCALHOST, MIB, RawScript, StreamServer, TIMEOUT, TestCa, echo, executor, failure, get, method,
    poll_once, response, send, send_within, spawn_counted_tls, spawn_h2_streams, spawn_raw_h2,
    trusting,
};

/// A hop on a session of its own, as a future that owns what it needs, so a
/// test can poll it and then spawn it.
fn owned(
    executor: &Arc<Executor>,
    request: HttpRequest,
    timeout: Duration,
) -> Pin<Box<impl Future<Output = HttpHop> + Send + 'static>> {
    let executor = executor.clone();
    Box::pin(async move { send_within(&executor, &request, timeout).await })
}

/// Sends `request` on a session of its own, on a task of its own.
fn spawned(executor: &Arc<Executor>, request: HttpRequest) -> JoinHandle<HttpHop> {
    tokio::spawn(owned(executor, request, TIMEOUT))
}

/// A `POST` of 32 MiB to `url`.
fn upload(url: String) -> HttpRequest {
    let mut request = get(url);
    request.method = method("POST");
    request.body = Some(HttpBody::Text("x".repeat(32 << 20)));
    request
}

/// Codex's scenario. The server's one stream is held, and a second hop runs
/// out of time waiting for it, in `connect`. Its request never reaches the
/// server, even once the stream is released, and the control hop after it
/// goes on the same connection.
async fn an_expired_hop_never_reaches_the_server(version: HttpVersion) {
    let ca = TestCa::generate();
    let mut server = spawn_h2_streams(Some(1), Some(&ca)).await;
    let addr = server.addr;
    let executor = Arc::new(trusting(&ca, version));

    let a = spawned(&executor, get(format!("https://{addr}/hold-first")));
    assert_eq!(server.arrival().await, "/hold-first");
    let b = send_within(
        &executor,
        &get(format!("https://{addr}/expired")),
        Duration::from_millis(300),
    )
    .await;
    assert_eq!(failure(&b), (FailureKind::Timeout, FailureStage::Connect));
    assert_eq!(b.connection, None);
    assert!(b.timing.setup.is_none());

    server.release("/hold-first");
    let a = a.await.unwrap();
    response(&a);
    let c = send(&executor, &get(format!("https://{addr}/control"))).await;
    response(&c);
    assert_eq!(c.connection, a.connection);
    assert!(c.timing.setup.is_none());
    assert_eq!(server.received(), ["/hold-first", "/control"]);
    assert_eq!(server.most_open(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expired_hop_never_reaches_the_server_over_http2() {
    an_expired_hop_never_reaches_the_server(HttpVersion::Http2).await;
}

/// ALPN picks `h2`, so the same holds under `auto`.
#[tokio::test(flavor = "multi_thread")]
async fn an_expired_hop_never_reaches_the_server_over_auto() {
    an_expired_hop_never_reaches_the_server(HttpVersion::Auto).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_waiting_hop_never_reaches_the_server() {
    let ca = TestCa::generate();
    let mut server = spawn_h2_streams(Some(1), Some(&ca)).await;
    let addr = server.addr;
    let executor = Arc::new(trusting(&ca, HttpVersion::Http2));

    let a = spawned(&executor, get(format!("https://{addr}/hold-first")));
    assert_eq!(server.arrival().await, "/hold-first");
    let mut b = owned(&executor, get(format!("https://{addr}/expired")), TIMEOUT);
    assert!(poll_once(&mut b).await.is_pending());
    drop(b);

    server.release("/hold-first");
    let a = a.await.unwrap();
    response(&a);
    let c = send(&executor, &get(format!("https://{addr}/control"))).await;
    response(&c);
    assert_eq!(c.connection, a.connection);
    assert!(c.timing.setup.is_none());
    assert_eq!(server.received(), ["/hold-first", "/control"]);
    assert_eq!(server.most_open(), 1);
}

/// Two hops are polled before any connection exists. The connection both go
/// on takes one stream until the server's first `SETTINGS`. The first hop
/// takes it, and the second runs out of time in `connect` without sending
/// its request.
async fn two_hops_at_once_send_one_request(version: HttpVersion) {
    let ca = TestCa::generate();
    let mut server = spawn_h2_streams(Some(1), Some(&ca)).await;
    let addr = server.addr;
    let executor = Arc::new(trusting(&ca, version));

    let mut a = owned(
        &executor,
        get(format!("https://{addr}/hold-first")),
        TIMEOUT,
    );
    let mut b = owned(
        &executor,
        get(format!("https://{addr}/expired")),
        Duration::from_millis(300),
    );
    assert!(poll_once(&mut a).await.is_pending());
    assert!(poll_once(&mut b).await.is_pending());
    let a = tokio::spawn(a);
    let b = b.await;
    assert_eq!(failure(&b), (FailureKind::Timeout, FailureStage::Connect));
    assert_eq!(b.connection, None);
    assert!(b.timing.setup.is_none());

    assert_eq!(server.arrival().await, "/hold-first");
    server.release("/hold-first");
    let a = a.await.unwrap();
    response(&a);
    let c = send(&executor, &get(format!("https://{addr}/control"))).await;
    response(&c);
    assert_eq!(c.connection, a.connection);
    assert_eq!(server.received(), ["/hold-first", "/control"]);
    assert_eq!(server.most_open(), 1);
}

/// Under `http2`, both hops wait on the one connect that makes the
/// connection.
#[tokio::test(flavor = "multi_thread")]
async fn two_hops_at_once_on_a_new_connection_send_one_request() {
    two_hops_at_once_send_one_request(HttpVersion::Http2).await;
}

/// Under `auto`, each hop starts a connect of its own. The first that ALPN
/// makes HTTP/2 becomes the origin's connection, and both hops take their
/// places at its gate in the order they asked.
#[tokio::test(flavor = "multi_thread")]
async fn two_hops_at_once_on_a_new_auto_connection_send_one_request() {
    two_hops_at_once_send_one_request(HttpVersion::Auto).await;
}

/// Under `auto`, two hops polled before any connection exists each start a
/// connect, and ALPN makes both HTTP/2. The first becomes the origin's
/// connection, and both hops go on it. The second closes without serving a
/// stream. The server answers only two handshakes, so a hop sent to a third
/// connection would never be answered.
#[tokio::test(flavor = "multi_thread")]
async fn auto_keeps_one_http2_connection_per_origin() {
    let ca = TestCa::generate();
    let accepted = Arc::new(AtomicUsize::new(0));
    let addr = spawn_counted_tls(
        echo,
        &[b"h2"],
        Some(2),
        ca.leaf_for_ip(LOCALHOST),
        accepted.clone(),
    )
    .await;
    let executor = Arc::new(trusting(&ca, HttpVersion::Auto));

    let mut a = owned(&executor, get(format!("https://{addr}/a")), TIMEOUT);
    let mut b = owned(&executor, get(format!("https://{addr}/b")), TIMEOUT);
    assert!(poll_once(&mut a).await.is_pending());
    assert!(poll_once(&mut b).await.is_pending());
    let (a, b) = tokio::join!(a, b);
    response(&a);
    response(&b);
    assert!(a.connection.is_some());
    assert_eq!(a.connection, b.connection);
    let setups = [&a, &b]
        .iter()
        .filter(|hop| hop.timing.setup.is_some())
        .count();
    assert_eq!(setups, 1);

    // The second connect may still be on its way to the server.
    tokio::time::timeout(TIMEOUT, async {
        while accepted.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both connects reached the server");
    assert_eq!(accepted.load(Ordering::SeqCst), 2);
}

/// Five hops queue in order on one connection whose server allows two
/// streams. Each freed stream goes to the hop that has waited longest.
#[tokio::test(flavor = "multi_thread")]
async fn the_gate_admits_in_order_and_never_past_the_limit() {
    let ca = TestCa::generate();
    let mut server = spawn_h2_streams(Some(2), Some(&ca)).await;
    let addr = server.addr;
    let executor = Arc::new(trusting(&ca, HttpVersion::Http2));

    let mut hops = Vec::new();
    for n in 1..=5 {
        let mut hop = owned(&executor, get(format!("https://{addr}/hold-{n}")), TIMEOUT);
        assert!(poll_once(&mut hop).await.is_pending());
        hops.push(hop);
    }
    let hops: Vec<_> = hops.into_iter().map(tokio::spawn).collect();

    // The first two take the two streams, in either order.
    let mut first = [server.arrival().await, server.arrival().await];
    first.sort();
    assert_eq!(first, ["/hold-1", "/hold-2"]);
    for (released, next) in [
        ("/hold-1", "/hold-3"),
        ("/hold-2", "/hold-4"),
        ("/hold-3", "/hold-5"),
    ] {
        server.release(released);
        assert_eq!(server.arrival().await, next, "after {released}");
    }
    server.release("/hold-4");
    server.release("/hold-5");
    for hop in hops {
        response(&hop.await.unwrap());
    }
    assert_eq!(server.most_open(), 2);
}

/// The server starts at one stream, and raises it to two while a hop waits
/// at the gate. The waiting hop then takes a stream while the first is still
/// open.
#[tokio::test(flavor = "multi_thread")]
async fn a_raised_limit_admits_a_queued_hop() {
    let mut server = spawn_raw_h2(RawScript::RaiseToTwo).await;
    let url = format!("http://{}/", server.addr);
    let executor = Arc::new(executor(HttpVersion::Http2));

    let a = spawned(&executor, get(url.clone()));
    assert_eq!(server.stream_opened().await, 1);
    let mut b = owned(&executor, get(url), TIMEOUT);
    assert!(poll_once(&mut b).await.is_pending());
    server.raise();
    let b = tokio::spawn(b);
    // Stream 1 is still held: the server answers only once it has two.
    assert_eq!(server.stream_opened().await, 3);

    let (a, b) = (a.await.unwrap(), b.await.unwrap());
    assert_eq!(response(&a).status, 200);
    assert_eq!(response(&b).status, 200);
    assert_eq!(a.connection, b.connection);
}

/// A server whose `SETTINGS` set no limit lets five hops have their streams
/// open together.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_with_no_limit_takes_five_streams_at_once() {
    let ca = TestCa::generate();
    let mut server = spawn_h2_streams(None, Some(&ca)).await;
    let addr = server.addr;
    let executor = Arc::new(trusting(&ca, HttpVersion::Http2));

    let paths: Vec<String> = (1..=5).map(|n| format!("/hold-{n}")).collect();
    let hops: Vec<_> = paths
        .iter()
        .map(|path| spawned(&executor, get(format!("https://{addr}{path}"))))
        .collect();
    let mut arrived = Vec::new();
    for _ in &paths {
        arrived.push(server.arrival().await);
    }
    arrived.sort();
    assert_eq!(arrived, paths);
    assert_eq!(server.most_open(), 5);

    for path in &paths {
        server.release(path);
    }
    for hop in hops {
        response(&hop.await.unwrap());
    }
}

/// Polls a hop to `/hold-first` and one to `path` once each, before any
/// connection exists, so both wait on the connect that makes it. Gives both
/// spawned, once the first one's request has arrived and the second waits at
/// the gate.
async fn two_on_one_connect(
    server: &mut StreamServer,
    executor: &Arc<Executor>,
    path: &str,
) -> (JoinHandle<HttpHop>, JoinHandle<HttpHop>) {
    let addr = server.addr;
    let mut a = owned(executor, get(format!("https://{addr}/hold-first")), TIMEOUT);
    let mut b = owned(executor, get(format!("https://{addr}{path}")), TIMEOUT);
    assert!(poll_once(&mut a).await.is_pending());
    assert!(poll_once(&mut b).await.is_pending());
    let (a, b) = (tokio::spawn(a), tokio::spawn(b));
    assert_eq!(server.arrival().await, "/hold-first");
    (a, b)
}

/// A `GOAWAY` drains the connection, and the hop waiting at its gate moves
/// to a new one, though it waited on the connect that made the draining
/// connection. The draining connection still serves its open stream.
#[tokio::test(flavor = "multi_thread")]
async fn a_goaway_moves_even_hops_whose_connect_made_the_connection() {
    let ca = TestCa::generate();
    let mut server = spawn_h2_streams(Some(1), Some(&ca)).await;
    let executor = Arc::new(trusting(&ca, HttpVersion::Http2));
    let (a, b) = two_on_one_connect(&mut server, &executor, "/").await;

    server.goaway();
    let b = b.await.unwrap();
    response(&b);
    assert!(b.timing.setup.is_some(), "a new connection");
    assert_eq!(server.accepted(), 2);

    server.release("/hold-first");
    let a = a.await.unwrap();
    response(&a);
    assert_ne!(a.connection, b.connection);
}

/// A connection that dies, with no `GOAWAY`, before it took the waiting
/// hop's request fails that hop in `connect` when the connection was fresh
/// for it, so a server that always breaks can't make it reconnect.
#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_connection_that_dies_fails_its_queued_hop_in_connect() {
    let ca = TestCa::generate();
    let mut server = spawn_h2_streams(Some(1), Some(&ca)).await;
    let executor = Arc::new(trusting(&ca, HttpVersion::Http2));
    let (a, b) = two_on_one_connect(&mut server, &executor, "/").await;

    server.cut();
    let b = b.await.unwrap();
    assert_eq!(
        failure(&b),
        (FailureKind::ConnectionClosed, FailureStage::Connect)
    );
    assert_eq!(b.connection, None);
    assert!(b.timing.setup.is_none());
    assert!(a.await.unwrap().outcome.is_err());
    assert_eq!(server.accepted(), 1);
}

/// A reused connection that dies before it took the waiting hop's request
/// moves that hop to a new connection.
#[tokio::test(flavor = "multi_thread")]
async fn a_reused_connection_that_dies_moves_its_queued_hop() {
    let ca = TestCa::generate();
    let mut server = spawn_h2_streams(Some(1), Some(&ca)).await;
    let addr = server.addr;
    let executor = Arc::new(trusting(&ca, HttpVersion::Http2));
    let warm = send(&executor, &get(format!("https://{addr}/"))).await;
    response(&warm);
    assert_eq!(server.arrival().await, "/");

    let a = spawned(&executor, get(format!("https://{addr}/hold-first")));
    assert_eq!(server.arrival().await, "/hold-first");
    let mut b = owned(&executor, get(format!("https://{addr}/")), TIMEOUT);
    assert!(poll_once(&mut b).await.is_pending());
    server.cut();

    let b = b.await;
    response(&b);
    assert!(b.timing.setup.is_some(), "a new connection");
    assert_ne!(b.connection, warm.connection);
    assert_eq!(server.accepted(), 2);
    assert!(a.await.unwrap().outcome.is_err());
}

/// A server that answers every new connection with `GOAWAY` fails the hop
/// once, before its deadline, with no reconnect storm. The hop is in
/// `connect` if the `GOAWAY` came before it handed over its request, and in
/// `headers` if h2 failed the stream it had just taken.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_refuses_every_connection_fails_the_hop_once() {
    let server = spawn_raw_h2(RawScript::GoAwayAtOnce).await;
    let timeout = Duration::from_secs(2);
    let hop = send_within(
        &executor(HttpVersion::Http2),
        &get(format!("http://{}/", server.addr)),
        timeout,
    )
    .await;
    let (kind, stage) = failure(&hop);
    assert_eq!(kind, FailureKind::ConnectionClosed);
    assert!(
        matches!(stage, FailureStage::Connect | FailureStage::Headers),
        "{stage:?}"
    );
    assert!(hop.timing.total < timeout, "{:?}", hop.timing.total);
    assert_eq!(server.accepted(), 1);
}

/// A hop dropped in the middle of its upload resets its stream, which stops
/// the upload, and its stream frees for the next hop on the connection.
#[tokio::test(flavor = "multi_thread")]
async fn dropping_an_http2_upload_stops_it() {
    let ca = TestCa::generate();
    let mut server = spawn_h2_streams(Some(1), Some(&ca)).await;
    let addr = server.addr;
    let executor = trusting(&ca, HttpVersion::Http2);

    let request = upload(format!("https://{addr}/hold-upload"));
    let mut hop = Box::pin(send(&executor, &request));
    tokio::select! {
        biased;
        _ = &mut hop => panic!("the upload ended before the server stopped reading"),
        () = server.upload_stopped() => {}
    }
    drop(hop);
    tokio::time::timeout(TIMEOUT, server.upload_reset())
        .await
        .expect("the server saw the stream reset");

    // The dropped hop took the connection's setup.
    let next = send(&executor, &get(format!("https://{addr}/"))).await;
    response(&next);
    assert!(next.timing.setup.is_none());
    assert_eq!(server.accepted(), 1);
}

/// A server that answers before it reads the upload ends the hop with its
/// answer. The upload stops there, and its stream frees for the next hop on
/// the connection.
#[tokio::test(flavor = "multi_thread")]
async fn an_early_http2_answer_ends_the_upload() {
    let ca = TestCa::generate();
    let server = spawn_h2_streams(Some(1), Some(&ca)).await;
    let addr = server.addr;
    let executor = trusting(&ca, HttpVersion::Http2);

    let early = send(&executor, &upload(format!("https://{addr}/early"))).await;
    let got = response(&early);
    assert_eq!(got.status, 200);
    assert_eq!(got.body, b"early");
    assert!(early.sizes.sent < 32 * MIB, "{}", early.sizes.sent);

    let next = send(&executor, &get(format!("https://{addr}/"))).await;
    response(&next);
    assert_eq!(next.connection, early.connection);
    assert!(next.timing.setup.is_none());
}
