//! The pool's origins: what each one holds, and how a hop gets a connection
//! from one.
//!
//! An origin holds its idle HTTP/1.1 connections and the hops waiting for a
//! connection, at most one HTTP/2 connection that takes new streams, and,
//! under `http2`, the hops waiting on its connect in progress. The pool's
//! module docs give the rules. The pool's lock guards every origin. It is
//! held only to change them, and connections are closed outside it.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use std::time::Instant;

use http::Uri;
use http::uri::{Authority, Scheme};
use tokio::runtime::Handle;
use vush_core::protocol::{Failure, FailureStage};

use super::gate::{Gate, Take};
use super::h1;
use super::h2::{self, H2Conn, Link};
use super::handoff::{Giver, Taker, handoff};
use super::{Pool, Shared};
use crate::classify::{classify, connector_failure};
use crate::connector::{Conn, ConnInfo};
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
    /// Hops waiting for a connection under `http1` or `auto`, each on a
    /// connect of its own, the longest-waiting first.
    waiters: VecDeque<Waiter>,
    /// The HTTP/2 connection that takes new streams.
    h2: Option<Arc<H2Conn>>,
    /// While an `http2` connect is in progress, the hops waiting on it, in
    /// the order they asked.
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

/// A connection a hop under `http1` or `auto` can send on.
pub(super) enum Lent {
    /// An HTTP/1.1 connection.
    H1(H1Conn),
    /// An HTTP/2 connection, with the hop's place at its gate.
    H2(Arc<H2Conn>, Take),
}

impl Lent {
    /// The HTTP/1.1 connection, if it is one, for the pool to place
    /// elsewhere. A place at an HTTP/2 gate passes to the next hop when it
    /// drops.
    fn h1(self) -> Option<H1Conn> {
        match self {
            Self::H1(conn) => Some(conn),
            Self::H2(..) => None,
        }
    }
}

/// What a waiting hop is given.
enum Offer {
    /// The result of the connect the hop started, so its connection is fresh
    /// for the hop.
    Fresh(Result<Lent, Failure>),
    /// A connection from the pool: an HTTP/1.1 one returned by another hop,
    /// or opened by a connect whose hop took another, or a place at the gate
    /// of an HTTP/2 connection another hop's connect made.
    Pooled(Lent),
}

impl Offer {
    /// The HTTP/1.1 connection offered, if any, for the pool to place
    /// elsewhere: a failed connect has none, and a place at an HTTP/2 gate
    /// passes on by itself.
    fn h1(self) -> Option<H1Conn> {
        match self {
            Self::Fresh(result) => result.ok().and_then(Lent::h1),
            Self::Pooled(lent) => lent.h1(),
        }
    }
}

/// What a connect under `http1` or `auto` made, as ALPN chose.
enum Made {
    H1(H1Conn),
    H2(Arc<H2Conn>),
}

/// What an HTTP/2 connect gives each hop that waited on it: the connection,
/// with the hop's place at its gate, or the connect's failure.
type Connected = Result<(Arc<H2Conn>, Take), Failure>;

/// What a checkout under `http1` or `auto` found under the lock.
enum Next {
    /// The origin's HTTP/2 connection.
    H2(Arc<H2Conn>),
    /// The idle connection returned last.
    Idle(H1Conn),
    /// No usable connection: the hop is queued as the waiter `id`.
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
    /// A connection for a hop to `key` under `http1` or `auto`, and whether
    /// it is fresh for the hop: from the connect the hop started. The hop
    /// takes the origin's HTTP/2 connection, or else its idle HTTP/1.1
    /// connection returned last. With neither, it starts a connect of its
    /// own, and takes whichever comes first: that connect's connection, an
    /// HTTP/1.1 one another hop returns, or a place at the gate of an HTTP/2
    /// one another hop's connect makes.
    pub(super) async fn checkout(&self, key: &Key, uri: &Uri) -> Result<(Lent, bool), Failure> {
        loop {
            let (id, taker) = match self.take_or_wait(key) {
                Next::H2(conn) => {
                    let take = conn.link.gate.take();
                    return Ok((Lent::H2(conn, take), false));
                }
                Next::Idle(conn) => return Ok((Lent::H1(conn), false)),
                Next::Wait(id, taker) => (id, taker),
            };
            self.connect(key.clone(), uri.clone(), id);
            let wait = Wait {
                pool: self,
                key,
                id,
                taker,
            };
            match wait.await {
                Some(Offer::Fresh(result)) => return result.map(|lent| (lent, true)),
                Some(Offer::Pooled(lent)) => return Ok((lent, false)),
                // The connect's task ended before it gave its result (see
                // `ConnectGuard`), so the hop connects anew.
                None => yield_before_retry().await,
            }
        }
    }

    /// Takes the origin's HTTP/2 connection for `key`, unless it is draining
    /// or has had no open stream for [`IDLE_TIMEOUT`]. Otherwise it takes the
    /// idle HTTP/1.1 connection returned last, after dropping the ones idle
    /// for [`IDLE_TIMEOUT`] and any that is closed. With none left, it queues
    /// the hop as a waiter, in the same lock, so no connection returned or
    /// made in between is missed.
    fn take_or_wait(&self, key: &Key) -> Next {
        loop {
            let (next, expired, stale) = {
                let mut state = self.lock();
                let State {
                    origins,
                    next_waiter,
                    ..
                } = &mut *state;
                let origin = origins.entry(key.clone()).or_default();
                let now = Instant::now();
                let stale = origin.take_stale_h2(now);
                let expired = origin.expire(now);
                let next = if let Some(conn) = &origin.h2 {
                    Next::H2(conn.clone())
                } else if let Some(idle) = origin.idle.pop() {
                    Next::Idle(idle.conn)
                } else {
                    let id = *next_waiter;
                    *next_waiter += 1;
                    let (giver, taker) = handoff();
                    origin.waiters.push_back(Waiter { id, giver });
                    Next::Wait(id, taker)
                };
                (next, expired, stale)
            };
            // Dropped outside the lock, where hyper and h2 close them.
            drop(expired);
            if let Some(stale) = stale {
                close_h2(stale);
            }
            match next {
                Next::Idle(conn) if conn.sender.is_closed() => {}
                next => return next,
            }
        }
    }

    /// Starts a connect to `uri` for the waiter `starter`, on a task of its
    /// own, so it finishes even when the hop takes another connection or is
    /// dropped. The connection speaks HTTP/2 when ALPN picked it, which
    /// never happens under `http1` or in plaintext, and HTTP/1.1 otherwise.
    fn connect(&self, key: Key, uri: Uri, starter: u64) {
        let connecting = self.shared.connector.connect_to(uri);
        let pool = Arc::downgrade(&self.shared);
        let guard = self.guard(&key, Owed::Starter(starter));
        tokio::spawn(async move {
            let result = match connecting.await {
                Ok(conn) if conn.is_h2() => open_h2(&pool, &key, conn).await.map(Made::H2),
                Ok(conn) => open_h1(conn).await.map(Made::H1),
                Err(error) => Err(connector_failure(error)),
            };
            // The result is given in this same poll.
            guard.disarm();
            // A connect that finishes after its pool is gone drops its
            // connection.
            if let Some(pool) = Pool::upgrade(&pool) {
                pool.deliver(key, starter, result);
            }
        });
    }

    /// Gives a finished connect's result to the hop that started it, if it
    /// still waits. Otherwise an HTTP/1.1 connection joins the pool, and a
    /// failure is dropped. An HTTP/2 connection goes to [`Pool::adopt`].
    fn deliver(&self, key: Key, starter: u64, result: Result<Made, Failure>) {
        let result = match result {
            Ok(Made::H2(conn)) => return self.adopt(key, starter, conn),
            Ok(Made::H1(conn)) => Ok(Lent::H1(conn)),
            Err(failure) => Err(failure),
        };
        let unclaimed = {
            let mut state = self.lock();
            let giver = state
                .origins
                .get_mut(&key)
                .and_then(|origin| origin.take_waiter(starter));
            match giver {
                Some(giver) => giver.give(Offer::Fresh(result)).err().and_then(Offer::h1),
                None => result.ok().and_then(Lent::h1),
            }
        };
        if let Some(conn) = unclaimed {
            self.put(key, conn);
        }
    }

    /// Takes in an HTTP/2 connection that the connect `starter` started made,
    /// under `auto`.
    ///
    /// - If the origin has no HTTP/2 connection, this one becomes its
    ///   connection, unless it is draining already, and every hop waiting
    ///   for the origin gets a place at its gate, in the order the hops
    ///   asked. It is fresh only for `starter`. Their own connects go on,
    ///   and the connections they make join the pool.
    /// - If the origin has one, which another connect made first, this one
    ///   closes without serving a stream, and `starter`, if it still waits,
    ///   gets a place at the gate of the origin's.
    /// - If the origin has none and this one is draining already, it goes
    ///   to `starter` alone, as fresh, and its gate turns the hop away (see
    ///   [`after_unsent`](super::after_unsent)).
    ///
    /// The first connection the pool stores starts its sweep.
    fn adopt(&self, key: Key, starter: u64, conn: Arc<H2Conn>) {
        let runtime = Handle::try_current().ok();
        let (offers, unused, stale, start) = {
            let mut state = self.lock();
            let origin = state.origins.entry(key).or_default();
            let stale = origin.take_stale_h2(Instant::now());
            let place = |conn: &Arc<H2Conn>| Lent::H2(conn.clone(), conn.link.gate.take());
            // Places taken under the lock, so no hop that finds the
            // connection gets ahead of these.
            let (offers, unused, stored): (Vec<_>, _, _) = match origin.h2.clone() {
                Some(first) => {
                    let offers = origin
                        .take_waiter(starter)
                        .map(|giver| (giver, Offer::Pooled(place(&first))))
                        .into_iter()
                        .collect();
                    (offers, Some(conn), false)
                }
                None if conn.link.is_draining() => {
                    let offers = origin
                        .take_waiter(starter)
                        .map(|giver| (giver, Offer::Fresh(Ok(place(&conn)))))
                        .into_iter()
                        .collect();
                    (offers, None, false)
                }
                None => {
                    origin.h2 = Some(conn.clone());
                    let offers = origin
                        .waiters
                        .drain(..)
                        .map(|waiter| {
                            let lent = place(&conn);
                            let offer = if waiter.id == starter {
                                Offer::Fresh(Ok(lent))
                            } else {
                                Offer::Pooled(lent)
                            };
                            (waiter.giver, offer)
                        })
                        .collect();
                    (offers, None, true)
                }
            };
            (
                offers,
                unused,
                stale,
                stored && state.claim_sweep(runtime.as_ref()),
            )
        };
        // Closed outside the lock, where h2 closes them once their last
        // handles are gone.
        stale.into_iter().chain(unused).for_each(close_h2);
        // A hop that stopped waiting drops its offer, and its place at the
        // gate passes to the next.
        for (giver, offer) in offers {
            drop(giver.give(offer));
        }
        if start && let Some(runtime) = runtime {
            self.start_sweeping(&runtime);
        }
    }

    /// A guard for a connect to `key` that owes its result to `owed`.
    fn guard(&self, key: &Key, owed: Owed) -> ConnectGuard {
        ConnectGuard {
            pool: Arc::downgrade(&self.shared),
            key: key.clone(),
            owed,
            armed: true,
        }
    }

    /// Lets go the hops a connect to `key` owed its result to, when its task
    /// ended without giving it: their givers are taken out of the origin
    /// and dropped outside the lock, which ends each hop's wait with
    /// nothing.
    fn let_go(&self, key: &Key, owed: Owed) {
        let gone = {
            let mut state = self.lock();
            let origin = state.origins.get_mut(key);
            match owed {
                Owed::Starter(id) => (origin.and_then(|origin| origin.take_waiter(id)), None),
                Owed::Waiters => (None, origin.and_then(|origin| origin.connecting.take())),
            }
        };
        drop(gone);
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
                        // The connect's task ended before it gave its result
                        // (see `ConnectGuard`), so the hop connects anew.
                        None => yield_before_retry().await,
                    }
                }
            }
        }
    }

    /// Starts `key`'s `http2` connect, on a task of its own, so it finishes,
    /// and serves every hop waiting on it, even when the hop that started it
    /// is dropped.
    fn connect_h2(&self, key: Key, uri: Uri) {
        let connecting = self.shared.connector.connect_to(uri);
        let pool = Arc::downgrade(&self.shared);
        let guard = self.guard(&key, Owed::Waiters);
        tokio::spawn(async move {
            let result = match connecting.await {
                Ok(conn) => open_h2(&pool, &key, conn).await,
                Err(error) => Err(connector_failure(error)),
            };
            // The result is given in this same poll.
            guard.disarm();
            // A connect that finishes after its pool is gone drops its
            // connection.
            if let Some(pool) = Pool::upgrade(&pool) {
                pool.settle(key, result);
            }
        });
    }

    /// Gives a finished `http2` connect's result to every hop that waited on
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
            match waiter.giver.give(Offer::Pooled(Lent::H1(conn))) {
                Ok(()) => return false,
                // The hop stopped waiting, so the next one gets it.
                Err(offer) => match offer.h1() {
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

/// Whom a connect in progress owes its result.
#[derive(Clone, Copy)]
enum Owed {
    /// The hop that started it, the waiter with this ID, under `http1` and
    /// `auto`.
    Starter(u64),
    /// Every hop waiting on the origin's connect in progress, under `http2`.
    Waiters,
}

/// A connect's hold on the hops it owes its result. Its task disarms it in
/// the poll that gives the result. A task that ends first, dropped with its
/// runtime or by a panic, drops it armed, which lets those hops go (see
/// [`Pool::let_go`]). Each then connects anew, instead of waiting until its
/// deadline on a connect that never ends, and under `http2` no later hop to
/// the origin waits on it either.
struct ConnectGuard {
    pool: Weak<Shared>,
    key: Key,
    owed: Owed,
    armed: bool,
}

impl ConnectGuard {
    /// The connect gives its result now, so its hops aren't let go.
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for ConnectGuard {
    fn drop(&mut self) {
        if self.armed
            && let Some(pool) = Pool::upgrade(&self.pool)
        {
            pool.let_go(&self.key, self.owed);
        }
    }
}

/// Yields once before a hop whose connect was let go connects anew. A
/// runtime that is shutting down drops each new task at once, and with it
/// the connect's guard, so without the yield the hop would retry within one
/// poll, without end.
async fn yield_before_retry() {
    tokio::task::yield_now().await;
}

/// Runs hyper's HTTP/1.1 handshake on `conn`. A failure is in `connect`.
async fn open_h1(conn: Conn) -> Result<H1Conn, Failure> {
    let info = conn.info();
    h1::handshake(conn)
        .await
        .map(|sender| H1Conn { sender, info })
        .map_err(|error| classify(&error, FailureStage::Connect))
}

/// Runs h2's handshake on `conn` and starts the connection's task, which
/// retires it from `key`'s origin in `pool` when it drains or ends. With
/// the pool gone, no hop waits at its gate. A failure is in `connect`.
async fn open_h2(pool: &Weak<Shared>, key: &Key, conn: Conn) -> Result<Arc<H2Conn>, Failure> {
    let info = conn.info();
    let (sender, connection) = h2::handshake(conn)
        .await
        .map_err(|error| classify(&error, FailureStage::Connect))?;
    let (pool, key) = (pool.clone(), key.clone());
    Ok(h2::start(sender, connection, info, move |link, goaway| {
        if let Some(pool) = Pool::upgrade(&pool) {
            pool.retire(&key, link, goaway);
        }
    }))
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
        if let Some(conn) = given.and_then(Offer::h1) {
            self.pool.put(self.key.clone(), conn);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Poll;
    use std::time::Duration;

    use bytes::Bytes;
    use http::Request;
    use rustls::RootCertStore;
    use tokio::runtime::{Builder, Runtime};
    use vush_core::plan::HttpVersion;
    use vush_core::protocol::{Failure, FailureKind, FailureStage};

    use crate::client::{Hop, new_pool};
    use crate::connector::{Connector, Resolver};
    use crate::stage::StageMarker;
    use crate::tls::{Verification, client_config};

    fn runtime() -> Runtime {
        Builder::new_multi_thread().enable_all().build().unwrap()
    }

    /// A connector for `version` whose first lookup never ends and whose
    /// later ones fail at once, with the count of lookups. Its limit is far
    /// off, so the first connect never ends by itself.
    fn flaky(version: HttpVersion) -> (Connector, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let resolver: Resolver = Arc::new(move |_, _| {
            let first = counted.fetch_add(1, Ordering::SeqCst) == 0;
            Box::pin(async move {
                if first {
                    std::future::pending().await
                } else {
                    Err(std::io::Error::other("no such host"))
                }
            })
        });
        let tls = client_config(Verification::Roots(RootCertStore::empty()), version);
        let connector = Connector::with_resolver(tls, version, Duration::from_secs(30), resolver);
        (connector, calls)
    }

    /// A hop starts its connect on a runtime that then shuts down, which
    /// drops the connect's task before it settles. The hop doesn't wait on
    /// it until its deadline: it starts one connect anew, whose lookup fails
    /// at once. Under `http2`, this frees the origin's connect in progress,
    /// which every later hop would otherwise have waited on.
    #[test]
    fn a_hop_whose_connect_was_dropped_connects_again() {
        for version in [HttpVersion::Http2, HttpVersion::Auto] {
            let (connector, lookups) = flaky(version);
            let pool = new_pool(connector, version);
            let marker = Arc::new(StageMarker::new());
            let sent = Arc::default();
            let mut handed = None;
            let request = Request::get("http://flaky.vush.test/")
                .body(Bytes::new())
                .unwrap();
            let hop = Hop {
                marker: &marker,
                sent: &sent,
                handed: &mut handed,
            };
            let mut hop = Box::pin(pool.send(request, hop));

            let doomed = runtime();
            doomed.block_on(async {
                let polled = std::future::poll_fn(|cx| Poll::Ready(hop.as_mut().poll(cx))).await;
                assert!(polled.is_pending(), "{version:?}");
                // The connect's task runs until its lookup, which never ends.
                tokio::time::timeout(Duration::from_secs(5), async {
                    while lookups.load(Ordering::SeqCst) == 0 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("the connect looked the host up");
            });
            drop(doomed);

            let outcome = runtime()
                .block_on(async { tokio::time::timeout(Duration::from_secs(5), hop).await })
                .unwrap_or_else(|_| panic!("{version:?}: the hop waited on the dropped connect"));
            assert_eq!(
                outcome.err(),
                Some(Failure {
                    kind: FailureKind::Dns,
                    stage: FailureStage::Connect,
                    message: "the host name didn't resolve",
                }),
                "{version:?}"
            );
            assert_eq!(lookups.load(Ordering::SeqCst), 2, "{version:?}");
        }
    }
}
