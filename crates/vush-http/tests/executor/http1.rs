//! Vush's own pool under `version: http1`: hops in a row share one
//! connection, a waiting hop takes the connection another hop returns, a
//! connection still writing its request is left alone, and idle
//! connections close with their pool.

use std::convert::Infallible;
use std::future::pending;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use http::Request;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use vush_core::plan::HttpVersion;
use vush_core::protocol::http::{HttpBody, HttpExecutor};

use crate::common::{
    LOCALHOST, ReplyFuture, TIMEOUT, TestCa, echo, executor, get, method, poll_once, reply,
    response, send, spawn_gated_h1, spawn_h1, trusting, write_all,
};

/// Serves HTTP/1.1 on `tcp` with `handler`, until the connection ends.
async fn serve(tcp: TcpStream, handler: fn(Request<Incoming>) -> ReplyFuture) {
    let service = service_fn(move |request| {
        let reply = handler(request);
        async move { Ok::<_, Infallible>(reply.await) }
    });
    let _ = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(tcp), service)
        .await;
}

/// Answers `200` with `connection: close`, so the server closes the
/// connection after the answer.
fn closing(_: Request<Incoming>) -> ReplyFuture {
    Box::pin(async { reply(200, &[("connection", "close")], "") })
}

/// Serves HTTP/1.1 in plaintext with `handler`, and counts the connections
/// it accepts.
async fn spawn_counted(
    handler: fn(Request<Incoming>) -> ReplyFuture,
) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind((LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = accepted.clone();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            count.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(serve(tcp, handler));
        }
    });
    (addr, accepted)
}

/// Serves HTTP/1.1 in plaintext with [`echo`], and sends on the channel
/// each time a connection ends: once the client closes it, since the server
/// never does.
async fn spawn_signalling() -> (SocketAddr, mpsc::UnboundedReceiver<()>) {
    let listener = TcpListener::bind((LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (ended_tx, ended) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let ended = ended_tx.clone();
            tokio::spawn(async move {
                serve(tcp, echo).await;
                let _ = ended.send(());
            });
        }
    });
    (addr, ended)
}

/// Reads into `buf`. 0 means the stream ended or failed.
async fn read_some(stream: &TcpStream, buf: &mut [u8]) -> usize {
    loop {
        if stream.readable().await.is_err() {
            return 0;
        }
        match stream.try_read(buf) {
            Ok(read) => return read,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => return 0,
        }
    }
}

/// Reads until the end of a request head, or until the stream ends.
async fn read_head(stream: &TcpStream) {
    let mut seen = Vec::new();
    let mut buf = [0; 4096];
    while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
        let read = read_some(stream, &mut buf).await;
        if read == 0 {
            return;
        }
        seen.extend_from_slice(&buf[..read]);
    }
}

/// Serves HTTP/1.1 in plaintext. Its first connection answers `200` with
/// `content-length: 0` as soon as it has the request head, then never reads
/// again. Every later connection answers each request with [`echo`].
async fn spawn_early_answer() -> SocketAddr {
    let listener = TcpListener::bind((LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let Ok((first, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(async move {
            read_head(&first).await;
            write_all(&first, b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").await;
            let _held = first;
            pending::<()>().await;
        });
        while let Ok((tcp, _)) = listener.accept().await {
            tokio::spawn(serve(tcp, echo));
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread")]
async fn fifty_hops_in_a_row_share_one_connection() {
    let addr = spawn_h1(echo).await;
    let executor = executor(HttpVersion::Http1);
    let mut session = executor.new_session();
    let request = get(format!("http://{addr}/"));

    let first = executor.send(&mut session, &request, TIMEOUT).await;
    response(&first);
    let connection = first
        .connection
        .expect("the first hop names its connection");
    assert!(first.timing.setup.is_some());

    for n in 1..50 {
        let hop = executor.send(&mut session, &request, TIMEOUT).await;
        response(&hop);
        assert_eq!(hop.connection, Some(connection), "hop {n}");
        assert_eq!(hop.timing.setup, None, "hop {n}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connection_closed_after_its_answer_is_not_reused() {
    let (addr, accepted) = spawn_counted(closing).await;
    let executor = executor(HttpVersion::Http1);
    let mut session = executor.new_session();
    let request = get(format!("http://{addr}/"));

    let first = executor.send(&mut session, &request, TIMEOUT).await;
    response(&first);
    let second = executor.send(&mut session, &request, TIMEOUT).await;
    response(&second);

    assert!(first.connection.is_some());
    assert!(second.connection.is_some());
    assert_ne!(second.connection, first.connection);
    assert!(second.timing.setup.is_some());
    assert_eq!(accepted.load(Ordering::SeqCst), 2);
}

/// The server answers one TLS handshake, so the second hop's own connect
/// never finishes: only the first hop's connection, once returned, can
/// serve it.
#[tokio::test(flavor = "multi_thread")]
async fn a_hop_waiting_for_a_connection_takes_one_returned_to_the_pool() {
    let (arrived_tx, mut arrived) = mpsc::unbounded_channel();
    let (release, released) = watch::channel(false);
    let handler = move |request: Request<Incoming>| {
        let slow = request.uri().path() == "/slow";
        let arrived = arrived_tx.clone();
        let mut released = released.clone();
        async move {
            if slow {
                let _ = arrived.send(());
                let _ = released.wait_for(|go| *go).await;
            }
            reply(200, &[], "")
        }
    };
    let ca = TestCa::generate();
    let addr = spawn_gated_h1(handler, &ca, 1).await;
    let executor = Arc::new(trusting(&ca, HttpVersion::Http1));

    let slow = {
        let executor = executor.clone();
        let request = get(format!("https://{addr}/slow"));
        tokio::spawn(async move { send(&executor, &request).await })
    };
    arrived.recv().await.expect("the server has /slow");

    // One poll makes the hop a waiter and starts its own connect.
    let mut session = executor.new_session();
    let request = get(format!("https://{addr}/"));
    let mut waiting = Box::pin(executor.send(&mut session, &request, TIMEOUT));
    assert!(poll_once(&mut waiting).await.is_pending());

    release.send(true).unwrap();
    let slow = slow.await.unwrap();
    let waited = waiting.await;
    response(&slow);
    response(&waited);
    assert!(slow.connection.is_some());
    assert_eq!(waited.connection, slow.connection);
    assert_eq!(waited.timing.setup, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_early_answer_to_an_upload_leaves_the_next_hop_a_new_connection() {
    let addr = spawn_early_answer().await;
    let executor = executor(HttpVersion::Http1);
    let mut session = executor.new_session();
    let mut upload = get(format!("http://{addr}/"));
    upload.method = method("POST");
    upload.body = Some(HttpBody::Text("x".repeat(32 << 20)));

    let early = executor.send(&mut session, &upload, TIMEOUT).await;
    assert_eq!(response(&early).status, 200);
    // The first connection is still writing the upload, which the server
    // never reads, so the next hop opens a connection of its own.
    let next = executor
        .send(&mut session, &get(format!("http://{addr}/")), TIMEOUT)
        .await;
    assert_eq!(response(&next).status, 200);

    assert!(early.connection.is_some());
    assert!(next.connection.is_some());
    assert_ne!(next.connection, early.connection);
    assert!(next.timing.setup.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_an_executor_closes_its_idle_connections() {
    let (addr, mut ended) = spawn_signalling().await;
    let executor = executor(HttpVersion::Http1);
    let hop = send(&executor, &get(format!("http://{addr}/"))).await;
    response(&hop);
    // The connection is idle in the executor's pool.
    assert!(ended.try_recv().is_err());

    drop(executor);
    tokio::time::timeout(TIMEOUT, ended.recv())
        .await
        .expect("the idle connection stayed open")
        .expect("the server ended");
}
