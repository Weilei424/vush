//! The pool's origins: what each one holds, and how a hop gets a connection
//! from one.
//!
//! An origin holds its idle HTTP/1.1 connections and the hops waiting for
//! one, and, under `http2`, its HTTP/2 connection and the hops waiting on
//! its connect in progress. The pool's module docs give the rules. The
//! pool's lock guards every origin. It is held only to change them, and
//! connections are closed outside it.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use http::Uri;
use http::uri::{Authority, Scheme};
use tokio::runtime::Handle;
use vush_core::protocol::{Failure, FailureStage};

use super::Pool;
use super::gate::{Gate, Take};
use super::h1;
use super::h2::{self, H2Conn, Link};
use super::handoff::{Giver, Taker, handoff};
use crate::classify::{classify, connector_failure};
use crate::connector::ConnInfo;
use crate::executor::IDLE_TIMEOUT;

/// An origin: a URI's scheme and authority.
pub(super) type Key = (Scheme, Authority);

/// The origins of a pool, under its lock.
#[derive(Default)]
pub(super) struct State {
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

pub(super) struct Idle {
    conn: H1Conn,
    since: Instant,
}

struct Waiter {
    id: u64,
    giver: Giver<Offer>,
}

/// An HTTP/1.1 connection: hyper's sender for it, and the connector's info.
pub(super) struct H1Conn {
    pub(super) sender: h1::Sender,
    pub(super) info: Arc<ConnInfo>,
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
    /// A connection for a hop to `key` under `http1`, and whether it is
    /// fresh for the hop: from the connect the hop started.
    pub(super) async fn checkout_h1(
        &self,
        key: &Key,
        uri: &Uri,
    ) -> Result<(H1Conn, bool), Failure> {
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
    pub(super) fn put(&self, key: Key, conn: H1Conn) {
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
    pub(super) async fn checkout_h2(
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
    pub(super) fn retire(&self, key: &Key, link: &Arc<Link>, goaway: bool) {
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
}

impl State {
    /// Takes out every idle HTTP/1.1 connection idle for [`IDLE_TIMEOUT`] by
    /// `at`, and every HTTP/2 connection that has had no open stream since
    /// then, for the caller to drop outside the lock, and drops the origins
    /// left with nothing.
    pub(super) fn sweep(&mut self, at: Instant) -> (Vec<Idle>, Vec<Arc<H2Conn>>) {
        let (mut closed, mut stale) = (Vec::new(), Vec::new());
        self.origins.retain(|_, origin| {
            closed.append(&mut origin.expire(at));
            stale.extend(origin.take_stale_h2(at));
            !origin.is_empty()
        });
        (closed, stale)
    }

    /// How many origins there are.
    #[cfg(test)]
    pub(super) fn origins(&self) -> usize {
        self.origins.len()
    }

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
pub(super) fn close_h2(conn: Arc<H2Conn>) {
    conn.link.drain(false);
    conn.link.gate.close();
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
