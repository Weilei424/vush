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

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::task::{Context, Poll, ready};
use std::time::Instant;

use bytes::Bytes;
use http::uri::{Authority, Scheme};
use http::{Request, Response, Uri};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use tokio::runtime::Handle;
use vush_core::plan::HttpVersion;
use vush_core::protocol::{ConnectionSetup, Failure, FailureKind, FailureStage};

use self::gate::{Gate, Take};
use self::h2::{Exchange, H2Body, H2Conn, Link};
use crate::body::CountingBody;
use crate::classify::{classify, connector_failure};
use crate::connector::{ConnInfo, Connector};
use crate::executor::IDLE_TIMEOUT;
use crate::stage::{StageMarker, Unwritten};
use crate::text;
use handoff::{Giver, Taker, handoff};

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

/// An origin: a URI's scheme and authority.
type Key = (Scheme, Authority);

#[derive(Default)]
struct State {
    origins: HashMap<Key, Origin>,
    /// The ID of the next hop to wait.
    next_waiter: u64,
    /// Whether the sweep's task has started.
    sweeping: bool,
}

#[derive(Default)]
struct Origin {
    /// Idle connections, in the order they were returned: the last was
    /// returned last, and the first has been idle longest.
    idle: Vec<Idle>,
    /// Hops waiting for a connection, the longest-waiting first.
    waiters: VecDeque<Waiter>,
    /// The HTTP/2 connection that takes new streams.
    h2: Option<Arc<H2Conn>>,
    /// While an HTTP/2 connect is in progress, the hops waiting on it, in the
    /// order they asked.
    connecting: Option<Vec<Giver<Connected>>>,
}

struct Idle {
    conn: H1Conn,
    since: Instant,
}

struct Waiter {
    id: u64,
    giver: Giver<Offer>,
}

/// An HTTP/1.1 connection: hyper's sender for it, and the connector's info.
struct H1Conn {
    sender: h1::Sender,
    info: Arc<ConnInfo>,
}

/// What a waiting hop is given.
enum Offer {
    /// The result of the connect the hop started, so its connection is fresh
    /// for the hop.
    Fresh(Result<H1Conn, Failure>),
    /// A connection from the pool: returned by another hop, or opened by a
    /// connect whose hop took another.
    Pooled(H1Conn),
}

impl Offer {
    /// The connection offered, if any: a failed connect has none.
    fn connection(self) -> Option<H1Conn> {
        match self {
            Self::Fresh(result) => result.ok(),
            Self::Pooled(conn) => Some(conn),
        }
    }
}

/// What an HTTP/2 connect gives each hop that waited on it: the connection,
/// with the hop's place at its gate, or the connect's failure.
type Connected = Result<(Arc<H2Conn>, Take), Failure>;

/// What a checkout found under the lock.
enum Next {
    /// The idle connection returned last.
    Idle(H1Conn),
    /// No usable idle connection: the hop is queued as the waiter `id`.
    Wait(u64, Taker<Offer>),
}

/// What an HTTP/2 checkout found under the lock.
enum NextH2 {
    /// The origin's connection.
    Conn(Arc<H2Conn>),
    /// No connection: the hop waits on the origin's connect, and starts it
    /// when `start`.
    Wait {
        taker: Taker<Connected>,
        start: bool,
    },
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

    /// A connection for a hop to `key` under `http1`, and whether it is
    /// fresh for the hop: from the connect the hop started.
    async fn checkout_h1(&self, key: &Key, uri: &Uri) -> Result<(H1Conn, bool), Failure> {
        loop {
            let (id, taker) = match self.take_or_wait(key) {
                Next::Idle(conn) => return Ok((conn, false)),
                Next::Wait(id, taker) => (id, taker),
            };
            self.connect_h1(key.clone(), uri.clone(), id);
            let wait = Wait {
                pool: self,
                key,
                id,
                taker,
            };
            match wait.await {
                Some(Offer::Fresh(result)) => return result.map(|conn| (conn, true)),
                Some(Offer::Pooled(conn)) => return Ok((conn, false)),
                // The pool dropped the waiter without giving it anything.
                None => {}
            }
        }
    }

    /// Takes the idle connection for `key` returned last, after dropping the
    /// ones idle for [`IDLE_TIMEOUT`] and any that is closed. With none left,
    /// it queues the hop as a waiter, in the same lock, so no connection
    /// returned in between is missed.
    fn take_or_wait(&self, key: &Key) -> Next {
        loop {
            let (next, expired) = {
                let mut state = self.lock();
                let State {
                    origins,
                    next_waiter,
                    ..
                } = &mut *state;
                let origin = origins.entry(key.clone()).or_default();
                let expired = origin.expire(Instant::now());
                let next = match origin.idle.pop() {
                    Some(idle) => Next::Idle(idle.conn),
                    None => {
                        let id = *next_waiter;
                        *next_waiter += 1;
                        let (giver, taker) = handoff();
                        origin.waiters.push_back(Waiter { id, giver });
                        Next::Wait(id, taker)
                    }
                };
                (next, expired)
            };
            // Dropped outside the lock, where hyper closes them.
            drop(expired);
            match next {
                Next::Idle(conn) if conn.sender.is_closed() => {}
                next => return next,
            }
        }
    }

    /// Starts a connect to `uri` for the waiter `starter`, on a task of its
    /// own, so it finishes even when the hop takes another connection or is
    /// dropped.
    fn connect_h1(&self, key: Key, uri: Uri, starter: u64) {
        let connecting = self.shared.connector.connect_to(uri);
        let pool = Arc::downgrade(&self.shared);
        tokio::spawn(async move {
            let result = match connecting.await {
                Ok(conn) => {
                    let info = conn.info();
                    h1::handshake(conn)
                        .await
                        .map(|sender| H1Conn { sender, info })
                        .map_err(|error| classify(&error, FailureStage::Connect))
                }
                Err(error) => Err(connector_failure(error)),
            };
            // A connect that finishes after its pool is gone drops its
            // connection.
            if let Some(pool) = Pool::upgrade(&pool) {
                pool.deliver(key, starter, result);
            }
        });
    }

    /// Gives a finished connect's result to the hop that started it, if it
    /// still waits. Otherwise the connection joins the pool, and a failure is
    /// dropped.
    fn deliver(&self, key: Key, starter: u64, result: Result<H1Conn, Failure>) {
        let unclaimed = {
            let mut state = self.lock();
            let giver = state
                .origins
                .get_mut(&key)
                .and_then(|origin| origin.take_waiter(starter));
            match giver {
                Some(giver) => giver
                    .give(Offer::Fresh(result))
                    .err()
                    .and_then(Offer::connection),
                None => result.ok(),
            }
        };
        if let Some(conn) = unclaimed {
            self.put(key, conn);
        }
    }

    /// Takes the waiter `id` out of `key`'s queue, if it is still there.
    fn forget(&self, key: &Key, id: u64) {
        let gone = {
            let mut state = self.lock();
            state
                .origins
                .get_mut(key)
                .and_then(|origin| origin.take_waiter(id))
        };
        drop(gone);
    }

    /// Gives `conn` back to `key`'s origin: to the longest-waiting hop, or
    /// else to the idle list. A closed connection is dropped. The first
    /// connection the pool stores starts its sweep.
    fn put(&self, key: Key, conn: H1Conn) {
        if conn.sender.is_closed() {
            return;
        }
        // A hop dropped outside a runtime can return a connection. The sweep
        // then starts with the next one stored on a runtime.
        let runtime = Handle::try_current().ok();
        let start = {
            let mut state = self.lock();
            state.place(key, conn) && state.claim_sweep(runtime.as_ref())
        };
        if start && let Some(runtime) = runtime {
            self.start_sweeping(&runtime);
        }
    }

    /// A connection for a hop to `key` under `http2`, the hop's place at its
    /// gate, and whether the connection is fresh for the hop: from the
    /// connect it waited on. With no connection taking streams, the hop
    /// waits on the origin's connect in progress, or starts one.
    async fn checkout_h2(
        &self,
        key: &Key,
        uri: &Uri,
    ) -> Result<(Arc<H2Conn>, Take, bool), Failure> {
        loop {
            let (next, stale) = {
                let mut state = self.lock();
                let origin = state.origins.entry(key.clone()).or_default();
                let stale = origin.take_stale_h2(Instant::now());
                let next = match &origin.h2 {
                    Some(conn) => NextH2::Conn(conn.clone()),
                    None => {
                        let (giver, taker) = handoff();
                        let start = origin.connecting.is_none();
                        origin.connecting.get_or_insert_default().push(giver);
                        NextH2::Wait { taker, start }
                    }
                };
                (next, stale)
            };
            if let Some(stale) = stale {
                close_h2(stale);
            }
            match next {
                NextH2::Conn(conn) => {
                    let take = conn.link.gate.take();
                    return Ok((conn, take, false));
                }
                NextH2::Wait { taker, start } => {
                    if start {
                        self.connect_h2(key.clone(), uri.clone());
                    }
                    match taker.await {
                        Some(Ok((conn, take))) => return Ok((conn, take, true)),
                        Some(Err(failure)) => return Err(failure),
                        // The connect's waiters were dropped unserved.
                        None => {}
                    }
                }
            }
        }
    }

    /// Starts `key`'s HTTP/2 connect, on a task of its own, so it finishes,
    /// and serves every hop waiting on it, even when the hop that started it
    /// is dropped.
    fn connect_h2(&self, key: Key, uri: Uri) {
        let connecting = self.shared.connector.connect_to(uri);
        let pool = Arc::downgrade(&self.shared);
        tokio::spawn(async move {
            let result = match connecting.await {
                Ok(conn) => {
                    let info = conn.info();
                    match h2::handshake(conn).await {
                        Ok((sender, connection)) => {
                            // With the pool gone, no hop waits at the gate.
                            let (pool, key) = (pool.clone(), key.clone());
                            Ok(h2::start(sender, connection, info, move |link, goaway| {
                                if let Some(pool) = Pool::upgrade(&pool) {
                                    pool.retire(&key, link, goaway);
                                }
                            }))
                        }
                        Err(error) => Err(classify(&error, FailureStage::Connect)),
                    }
                }
                Err(error) => Err(connector_failure(error)),
            };
            // A connect that finishes after its pool is gone drops its
            // connection.
            if let Some(pool) = Pool::upgrade(&pool) {
                pool.settle(key, result);
            }
        });
    }

    /// Gives a finished HTTP/2 connect's result to every hop that waited on
    /// it. The connection becomes the origin's, unless it is draining
    /// already, and each hop gets its place at the connection's gate, in the
    /// order the hops asked. The first connection the pool stores starts its
    /// sweep.
    fn settle(&self, key: Key, result: Result<Arc<H2Conn>, Failure>) {
        let runtime = Handle::try_current().ok();
        let (offers, replaced, start) = {
            let mut state = self.lock();
            let origin = state.origins.entry(key).or_default();
            let waiters = origin.connecting.take().unwrap_or_default();
            // Only one connect runs at a time, so nothing is replaced.
            let (stored, replaced) = match &result {
                Ok(conn) if !conn.link.is_draining() => (true, origin.h2.replace(conn.clone())),
                _ => (false, None),
            };
            // Places taken under the lock, so no hop that finds the
            // connection gets ahead of these.
            let offers: Vec<_> = waiters
                .into_iter()
                .map(|giver| {
                    let offer = match &result {
                        Ok(conn) => Ok((conn.clone(), conn.link.gate.take())),
                        Err(failure) => Err(*failure),
                    };
                    (giver, offer)
                })
                .collect();
            (
                offers,
                replaced,
                stored && state.claim_sweep(runtime.as_ref()),
            )
        };
        drop(replaced);
        // A hop that stopped waiting drops its offer, and its place at the
        // gate passes to the next.
        for (giver, offer) in offers {
            drop(giver.give(offer));
        }
        if start && let Some(runtime) = runtime {
            self.start_sweeping(&runtime);
        }
    }

    /// Stops an HTTP/2 connection taking streams, in an order that lets no
    /// hop find it again: it is marked draining, leaves its origin, and then
    /// its gate closes, which sends the hops waiting there on. `goaway` says
    /// whether the server's `GOAWAY` was the cause.
    fn retire(&self, key: &Key, link: &Arc<Link>, goaway: bool) {
        link.drain(goaway);
        let gone = {
            let mut state = self.lock();
            state
                .origins
                .get_mut(key)
                .and_then(|origin| origin.take_h2(link))
        };
        // Dropped outside the lock, where h2 may close it.
        drop(gone);
        link.gate.close();
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
        let (closed, stale) = {
            let mut state = self.lock();
            let (mut closed, mut stale) = (Vec::new(), Vec::new());
            state.origins.retain(|_, origin| {
                closed.append(&mut origin.expire(at));
                stale.extend(origin.take_stale_h2(at));
                !origin.is_empty()
            });
            (closed, stale)
        };
        // Dropped outside the lock, where hyper and h2 close them.
        drop(closed);
        stale.into_iter().for_each(close_h2);
    }

    /// How many origins the pool holds.
    #[cfg(test)]
    fn origins(&self) -> usize {
        self.lock().origins.len()
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

impl State {
    /// Gives `conn` to the longest-waiting hop for `key`, or else stores it
    /// as idle from now. It gives whether the connection was stored.
    fn place(&mut self, key: Key, mut conn: H1Conn) -> bool {
        let origin = self.origins.entry(key).or_default();
        while let Some(waiter) = origin.waiters.pop_front() {
            match waiter.giver.give(Offer::Pooled(conn)) {
                Ok(()) => return false,
                // The hop stopped waiting, so the next one gets it.
                Err(offer) => match offer.connection() {
                    Some(back) => conn = back,
                    None => return false,
                },
            }
        }
        origin.idle.push(Idle {
            conn,
            since: Instant::now(),
        });
        true
    }

    /// Whether a connection just stored starts the pool's sweep: the first
    /// stored on a runtime does.
    fn claim_sweep(&mut self, runtime: Option<&Handle>) -> bool {
        let start = !self.sweeping && runtime.is_some();
        self.sweeping |= start;
        start
    }
}

impl Origin {
    /// Removes the connections idle for [`IDLE_TIMEOUT`] by `at`. They come
    /// first, because the list is in the order they were returned.
    fn expire(&mut self, at: Instant) -> Vec<Idle> {
        let end = self
            .idle
            .partition_point(|idle| at.saturating_duration_since(idle.since) >= IDLE_TIMEOUT);
        self.idle.drain(..end).collect()
    }

    /// Takes the waiter `id` out of the queue, if it is still there.
    fn take_waiter(&mut self, id: u64) -> Option<Giver<Offer>> {
        let at = self.waiters.iter().position(|waiter| waiter.id == id)?;
        self.waiters.remove(at).map(|waiter| waiter.giver)
    }

    /// Takes out the HTTP/2 connection if it can't take a stream: it is
    /// draining, or it has had no open stream for [`IDLE_TIMEOUT`] by `at`.
    fn take_stale_h2(&mut self, at: Instant) -> Option<Arc<H2Conn>> {
        let stale = self
            .h2
            .as_ref()
            .is_some_and(|conn| conn.link.is_draining() || idle_too_long(&conn.link.gate, at));
        if stale { self.h2.take() } else { None }
    }

    /// Takes out the HTTP/2 connection if it is the one `link` belongs to.
    fn take_h2(&mut self, link: &Arc<Link>) -> Option<Arc<H2Conn>> {
        let this = self
            .h2
            .as_ref()
            .is_some_and(|conn| Arc::ptr_eq(&conn.link, link));
        if this { self.h2.take() } else { None }
    }

    /// Whether the origin holds nothing, so the sweep can drop it.
    fn is_empty(&self) -> bool {
        self.idle.is_empty()
            && self.waiters.is_empty()
            && self.h2.is_none()
            && self.connecting.is_none()
    }
}

/// Whether `gate` has had no open stream for [`IDLE_TIMEOUT`] by `at`.
fn idle_too_long(gate: &Gate, at: Instant) -> bool {
    gate.idle_since()
        .is_some_and(|since| at.saturating_duration_since(since) >= IDLE_TIMEOUT)
}

/// Closes an HTTP/2 connection already out of its origin: it is marked
/// draining and its gate closes. h2 closes it once its last handle and its
/// streams are gone.
fn close_h2(conn: Arc<H2Conn>) {
    conn.link.drain(false);
    conn.link.gate.close();
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

/// A hop's wait for a connection. Dropping it, once the hop got one or
/// ended, takes the hop out of the queue, and a connection given to it but
/// not taken goes back to the pool.
struct Wait<'a> {
    pool: &'a Pool,
    key: &'a Key,
    id: u64,
    taker: Taker<Offer>,
}

impl Future for Wait<'_> {
    type Output = Option<Offer>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Offer>> {
        Pin::new(&mut self.taker).poll(cx)
    }
}

impl Drop for Wait<'_> {
    fn drop(&mut self) {
        let given = self.taker.close();
        self.pool.forget(self.key, self.id);
        if let Some(conn) = given.and_then(Offer::connection) {
            self.pool.put(self.key.clone(), conn);
        }
    }
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
