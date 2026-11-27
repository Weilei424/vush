//! Vush's own connection pool.
//!
//! A pool groups its connections by origin: the URI's scheme and authority,
//! compared as `http`'s types compare them. It serves one version mode,
//! `http1` or `http2`, so far.
//!
//! Under `http1`, each origin holds its idle HTTP/1.1 connections and the
//! hops waiting for one.
//!
//! - **Checkout.** A hop takes the idle connection returned last, skipping
//!   any that is closed or idle for [`IDLE_TIMEOUT`] or more, and waits
//!   until hyper is ready for a request. If none is usable, it starts a
//!   connect of its own on a task, and takes whichever comes first: that
//!   connect's connection, or one another hop returns to the origin. A
//!   connect the hop didn't take still finishes, and its connection goes to
//!   the longest-waiting hop or to the idle list.
//! - **Sending.** The request is handed to the connection with hyper's
//!   `try_send_request`, and the hop's [`Handed`] names the connection from
//!   then on. A request that never reached the connection goes to another
//!   one when this one was reused. On a connection fresh for the hop, it
//!   fails as `connection_closed` in `connect` (see [`after_unsent`]).
//! - **Returning.** When a body yields its end, its connection goes to the
//!   longest-waiting hop, or else to the idle list. One still writing its
//!   request, because the server answered early, goes back from a task once
//!   hyper is ready. A body dropped before its end drops its connection,
//!   which hyper then closes.
//!
//! Under `http2`, each origin holds at most one HTTP/2 connection that takes
//! new streams, and the hops waiting on its connect in progress.
//!
//! - **Checkout.** A hop takes the origin's connection, unless it is draining
//!   or has had no open stream for [`IDLE_TIMEOUT`]. With none, it waits on
//!   the origin's connect in progress, or starts one on a task of its own.
//!   The connect serves every hop waiting on it, even when the hop that
//!   started it is dropped, and each gets the connect's own failure. Its
//!   connection is fresh for those hops.
//! - **Sending.** The hop waits at the connection's stream gate, in
//!   `connect` and naming no connection. In the poll that gives it a slot,
//!   it hands h2 its request, so a hop that ends while it waits never sends
//!   it. The hops that waited on a connect take their places at its gate in
//!   the order they asked.
//! - **Draining.** A `GOAWAY`, or a connection error, retires the connection:
//!   it leaves its origin and its gate closes, while its open streams go on.
//!   A hop the gate turns away, or whose request h2 turns down, never sent
//!   its request, and goes to another connection, or fails, as
//!   [`after_unsent`] says.
//!
//! A task started with the pool's first stored connection sweeps the pool
//! every [`IDLE_TIMEOUT`]. Like every task the pool starts, it holds the pool
//! weakly, so dropping the pool drops its connections, which hyper and h2
//! then close.
//!
//! A pool's lock is never held across an `.await`, or while calling into
//! hyper or h2: connections are checked, cloned and dropped outside it.

mod gate;
mod h1;
mod h2;
mod handoff;
mod origin;

use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::task::{Context, Poll, ready};
use std::time::Instant;

use bytes::Bytes;
use http::{Request, Response, Uri};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use tokio::runtime::Handle;
use vush_core::plan::HttpVersion;
use vush_core::protocol::{ConnectionSetup, Failure, FailureKind, FailureStage};

use self::h2::{Exchange, H2Body};
use self::origin::{H1Conn, Key, State, close_h2};
use crate::body::CountingBody;
use crate::classify::classify;
use crate::connector::{ConnInfo, Connector};
use crate::executor::IDLE_TIMEOUT;
use crate::stage::{StageMarker, Unwritten};
use crate::text;

/// The error of a response body.
pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// What a hop lends the pool for one `send`.
pub(crate) struct Hop<'a> {
    /// The hop's stage. Under HTTP/1.1, the request's mark moves it to
    /// `headers` when hyper writes the head. Under HTTP/2, the pool moves it
    /// when h2 takes the request.
    pub marker: &'a Arc<StageMarker>,
    /// The body bytes handed to hyper or h2 so far.
    pub sent: &'a Arc<AtomicU64>,
    /// The connection the request is handed to, set by the pool.
    pub handed: &'a mut Option<Handed>,
}

/// The connection a request was handed to.
pub(crate) struct Handed {
    pub info: Arc<ConnInfo>,
    /// The connection's setup, if this hop was the first to hand it a
    /// request.
    pub setup: Option<ConnectionSetup>,
}

/// What follows for a request that never reached its connection: `None` to
/// try another connection, or the hop's failure. `drained` is for a
/// connection that turned the request down because of a `GOAWAY`, after it
/// had opened at least one stream, so it is always false under HTTP/1.1.
///
/// A drained connection moves the hop. Otherwise a connection fresh for the
/// hop fails it, as `connection_closed` in `connect`, so a server that
/// always breaks, or that answers every new connection with `GOAWAY`, can't
/// make the hop reconnect until its deadline, and a reused one moves it.
pub(super) fn after_unsent(fresh: bool, drained: bool) -> Option<Failure> {
    (fresh && !drained).then_some(Failure {
        kind: FailureKind::ConnectionClosed,
        stage: FailureStage::Connect,
        message: text::CONNECTION_CLOSED,
    })
}

/// A pool of connections. Clones share it.
#[derive(Clone)]
pub(crate) struct Pool {
    shared: Arc<Shared>,
}

/// A pool for connections from `connector`, which must have been built for
/// `version`. Only `http1` and `http2` pools are Vush's own so far.
pub(crate) fn new_pool(connector: Connector, version: HttpVersion) -> Pool {
    debug_assert_ne!(
        version,
        HttpVersion::Auto,
        "auto pools aren't built here yet"
    );
    Pool {
        shared: Arc::new(Shared {
            connector,
            version,
            state: Mutex::default(),
        }),
    }
}

/// What a pool's clones share. It holds hosts, so it has no `Debug`.
struct Shared {
    connector: Connector,
    version: HttpVersion,
    state: Mutex<State>,
}

impl Pool {
    /// Sends `request` on a connection to its origin and waits for the head.
    /// `hop.sent` counts the body bytes handed over, and `hop.handed` names
    /// the connection from the moment the request is handed to it, with the
    /// connection's setup if no hop took it first. The hop's marker moves to
    /// `headers` then: under HTTP/1.1 through the request's mark, when hyper
    /// writes the head, and under HTTP/2 when h2 takes the request.
    ///
    /// A connect failure is the connector's own. A failure after the
    /// request was handed over is classified in `headers`.
    pub(crate) async fn send(
        &self,
        request: Request<Bytes>,
        hop: Hop<'_>,
    ) -> Result<Response<ResponseBody>, Failure> {
        let Some(key) = origin(request.uri()) else {
            // `prepare` makes every URL absolute, with a host.
            return Err(Failure {
                kind: FailureKind::InvalidRequest,
                stage: FailureStage::Prepare,
                message: text::URL_NO_HOST,
            });
        };
        if self.shared.version == HttpVersion::Http2 {
            self.send_h2(key, request, hop).await
        } else {
            self.send_h1(key, request, hop).await
        }
    }

    /// Sends `request` over HTTP/1.1. The body is wrapped to count into
    /// `hop.sent`, and the request carries a mark that moves `hop.marker` to
    /// `headers` when hyper writes the head.
    async fn send_h1(
        &self,
        key: Key,
        request: Request<Bytes>,
        hop: Hop<'_>,
    ) -> Result<Response<ResponseBody>, Failure> {
        let Hop {
            marker,
            sent,
            handed,
        } = hop;
        let uri = request.uri().clone();
        let mut request = request.map(|body| CountingBody::new(body, sent.clone()));
        h1::shape(&mut request);
        request
            .extensions_mut()
            .insert(Unwritten::new(marker.clone()));
        loop {
            let (conn, fresh) = self.checkout_h1(&key, &uri).await?;
            let held = Held {
                pool: self,
                key: &key,
                conn: Some(conn),
            };
            let Some(mut conn) = held.ready().await else {
                // It closed before it took the request.
                unsent_h1(&request, fresh)?;
                continue;
            };
            *handed = Some(Handed {
                info: conn.info.clone(),
                setup: conn.info.take_setup(),
            });
            match conn.sender.send(request).await {
                Ok(head) => {
                    let lease = Lease {
                        pool: Arc::downgrade(&self.shared),
                        key,
                        conn,
                    };
                    return Ok(head.map(|body| {
                        ResponseBody::H1(H1Body {
                            body,
                            lease: Some(lease),
                        })
                    }));
                }
                Err(h1::Refused::Unsent(back)) => {
                    *handed = None;
                    request = *back;
                    unsent_h1(&request, fresh)?;
                }
                Err(h1::Refused::Failed(error)) => {
                    return Err(classify(&error, FailureStage::Headers));
                }
            }
        }
    }

    /// Sends `request` over HTTP/2. The hop waits at its connection's gate,
    /// in `connect`, and hands h2 the request in the poll that gives it a
    /// slot. Its body then goes out inside the hop, counted into `hop.sent`.
    async fn send_h2(
        &self,
        key: Key,
        request: Request<Bytes>,
        hop: Hop<'_>,
    ) -> Result<Response<ResponseBody>, Failure> {
        let Hop {
            marker,
            sent,
            handed,
        } = hop;
        loop {
            let (conn, take, fresh) = self.checkout_h2(&key, request.uri()).await?;
            let Ok(slot) = take.await else {
                // The connection was retired while the hop waited.
                if let Some(failure) = after_unsent(fresh, conn.link.drained()) {
                    return Err(failure);
                }
                continue;
            };
            // In the same poll as the slot, so the hop can't end in between.
            let opened = match conn.open(&request) {
                Ok(opened) => opened,
                Err(refused) => {
                    // Closed first, so the slot goes to no other hop.
                    self.retire(&key, &conn.link, refused.goaway);
                    drop(slot);
                    if let Some(failure) = after_unsent(fresh, conn.link.drained()) {
                        return Err(failure);
                    }
                    continue;
                }
            };
            *handed = Some(Handed {
                info: conn.info.clone(),
                setup: conn.info.take_setup(),
            });
            marker.set(FailureStage::Headers);
            drop(conn);
            let exchange = Exchange::new(opened, request.into_body(), sent.clone(), slot);
            let head = exchange
                .await
                .map_err(|error| classify(&error, FailureStage::Headers))?;
            return Ok(head.map(ResponseBody::H2));
        }
    }

    /// Starts the task that sweeps the pool every [`IDLE_TIMEOUT`]. It holds
    /// the pool weakly, so it ends once the pool is gone.
    fn start_sweeping(&self, runtime: &Handle) {
        let pool = Arc::downgrade(&self.shared);
        runtime.spawn(async move {
            loop {
                tokio::time::sleep(IDLE_TIMEOUT).await;
                let Some(pool) = Pool::upgrade(&pool) else {
                    return;
                };
                pool.sweep(Instant::now());
            }
        });
    }

    /// Closes every idle HTTP/1.1 connection idle for [`IDLE_TIMEOUT`] by
    /// `at`, and every HTTP/2 connection that has had no open stream since
    /// then, and drops the origins left with nothing.
    pub(crate) fn sweep(&self, at: Instant) {
        let (closed, stale) = self.lock().sweep(at);
        // Dropped outside the lock, where hyper and h2 close them.
        drop(closed);
        stale.into_iter().for_each(close_h2);
    }

    /// How many origins the pool holds.
    #[cfg(test)]
    fn origins(&self) -> usize {
        self.lock().origins()
    }

    /// The pool `shared` points to, if it is still there.
    fn upgrade(shared: &Weak<Shared>) -> Option<Self> {
        shared.upgrade().map(|shared| Self { shared })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// The origin of `uri`, if it has a scheme and an authority.
fn origin(uri: &Uri) -> Option<Key> {
    Some((uri.scheme()?.clone(), uri.authority()?.clone()))
}

/// What follows for an HTTP/1.1 request that never reached a connection:
/// `Ok` to try another connection, or the hop's failure. Before a failure,
/// the request's mark is disarmed, so the hop stays in `connect` when the
/// request drops.
fn unsent_h1(request: &Request<CountingBody>, fresh: bool) -> Result<(), Failure> {
    let Some(failure) = after_unsent(fresh, false) else {
        return Ok(());
    };
    if let Some(mark) = request.extensions().get::<Unwritten>() {
        mark.disarm();
    }
    Err(failure)
}

/// A connection a hop holds until hyper is ready for its request. If the
/// hop is dropped first, the connection goes back to the pool.
struct Held<'a> {
    pool: &'a Pool,
    key: &'a Key,
    conn: Option<H1Conn>,
}

impl Held<'_> {
    /// The connection, once hyper is ready for a request, or `None` when it
    /// closed first.
    async fn ready(mut self) -> Option<H1Conn> {
        let ready = match &mut self.conn {
            Some(conn) => conn.sender.ready().await.is_ok(),
            None => false,
        };
        let conn = self.conn.take();
        if ready { conn } else { None }
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.pool.put(self.key.clone(), conn);
        }
    }
}

/// A response's body. An HTTP/1.1 connection goes back to the pool at the
/// end of its body, and an HTTP/2 stream's slot frees then.
pub(crate) enum ResponseBody {
    /// An HTTP/1.1 body.
    H1(H1Body),
    /// An HTTP/2 body.
    H2(H2Body),
}

/// An HTTP/1.1 body, with its connection until the end.
pub(crate) struct H1Body {
    body: Incoming,
    lease: Option<Lease>,
}

/// The connection a hop is reading a response on, and where it goes back.
struct Lease {
    pool: Weak<Shared>,
    key: Key,
    conn: H1Conn,
}

impl Lease {
    /// Gives the connection back once its body was read to the end. A
    /// connection still writing its request, because the server answered
    /// early, goes back from a task once hyper is ready, so no hop waits
    /// behind a write the server may never read. If the stall limit fails
    /// that write, the task drops the connection.
    fn give_back(self) {
        let Self {
            pool,
            key,
            mut conn,
        } = self;
        if !conn.info.write_waiting() {
            if let Some(pool) = Pool::upgrade(&pool) {
                pool.put(key, conn);
            }
            return;
        }
        // A body is read inside its hop, on the runtime.
        let Ok(runtime) = Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            if conn.sender.ready().await.is_ok()
                && let Some(pool) = Pool::upgrade(&pool)
            {
                pool.put(key, conn);
            }
        });
    }
}

impl Body for ResponseBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        match self.get_mut() {
            Self::H1(h1) => {
                let polled = ready!(Pin::new(&mut h1.body).poll_frame(cx));
                match &polled {
                    None => {
                        if let Some(lease) = h1.lease.take() {
                            lease.give_back();
                        }
                    }
                    // A failed body drops its connection.
                    Some(Err(_)) => h1.lease = None,
                    Some(Ok(_)) => {}
                }
                Poll::Ready(polled.map(|frame| frame.map_err(BoxError::from)))
            }
            Self::H2(h2) => h2
                .poll_frame(cx)
                .map(|polled| polled.map(|frame| frame.map_err(BoxError::from))),
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            Self::H1(h1) => h1.body.size_hint(),
            // The executor reads every frame, so it needs no hint.
            Self::H2(_) => SizeHint::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::time::{Duration, Instant};

    use bytes::Bytes;
    use http::{Request, Response};
    use http_body_util::{BodyExt, Full};
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use rustls::RootCertStore;
    use tokio::net::TcpListener;
    use vush_core::plan::HttpVersion;
    use vush_core::protocol::{Failure, FailureKind, FailureStage};

    use super::*;
    use crate::connector::Connector;
    use crate::executor::IDLE_TIMEOUT;
    use crate::stage::StageMarker;
    use crate::tls::{Verification, client_config};

    #[test]
    fn an_unsent_request_fails_only_on_a_fresh_connection() {
        assert_eq!(
            after_unsent(true, false),
            Some(Failure {
                kind: FailureKind::ConnectionClosed,
                stage: FailureStage::Connect,
                message: "the connection closed before the response was complete",
            })
        );
        assert_eq!(after_unsent(false, false), None);
    }

    /// A connection that turned the request down because of a `GOAWAY`, after
    /// it had opened a stream, moves the hop either way.
    #[test]
    fn a_drained_connection_always_moves_the_hop() {
        assert_eq!(after_unsent(true, true), None);
        assert_eq!(after_unsent(false, true), None);
    }

    /// An HTTP/1.1 server on 127.0.0.1 that answers every request `200`.
    async fn spawn_h1() -> SocketAddr {
        let listener = TcpListener::bind((IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let service = service_fn(|_| async {
                        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"ok"))))
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(tcp), service)
                        .await;
                });
            }
        });
        addr
    }

    /// A `version` pool for plaintext servers.
    pub(super) fn pool(version: HttpVersion) -> Pool {
        let tls = client_config(Verification::Roots(RootCertStore::empty()), version);
        let connector = Connector::new(tls, version, Duration::from_secs(5));
        new_pool(connector, version)
    }

    /// Sends a `GET` to `addr` through `pool` and reads the body to its end,
    /// which gives the connection back. It gives what the request was
    /// handed to.
    pub(super) async fn hop(pool: &Pool, addr: SocketAddr) -> Handed {
        let marker = Arc::new(StageMarker::new());
        marker.set(FailureStage::Connect);
        let sent = Arc::new(AtomicU64::new(0));
        let mut handed = None;
        let request = Request::get(format!("http://{addr}/"))
            .body(Bytes::new())
            .unwrap();
        let hop = Hop {
            marker: &marker,
            sent: &sent,
            handed: &mut handed,
        };
        let response = pool.send(request, hop).await.expect("the hop succeeds");
        let mut body = response.into_body();
        while let Some(frame) = body.frame().await {
            frame.expect("the body arrives whole");
        }
        handed.expect("the request was handed to a connection")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_sweep_closes_connections_idle_for_90_s() {
        let (a, b) = (spawn_h1().await, spawn_h1().await);
        let pool = pool(HttpVersion::Http1);
        for addr in [a, b] {
            assert!(hop(&pool, addr).await.setup.is_some());
        }
        let t0 = Instant::now();
        assert_eq!(pool.origins(), 2);

        pool.sweep(t0 + Duration::from_secs(89));
        assert_eq!(pool.origins(), 2, "both connections were kept");

        pool.sweep(t0 + IDLE_TIMEOUT + Duration::from_millis(1));
        assert_eq!(pool.origins(), 0);
        for addr in [a, b] {
            assert!(hop(&pool, addr).await.setup.is_some(), "a new connection");
        }
    }
}
