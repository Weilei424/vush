//! The stream gate in front of each HTTP/2 connection.
//!
//! Its count is the number of streams Vush has open on the connection: from
//! the moment a hop takes a slot, which it does in the same poll as it hands
//! h2 its request, until the hop drops the slot, after the stream's last h2
//! handle. A hop takes a slot only while the count is below the server's
//! limit, so h2 never has to hold a request until a stream frees.
//!
//! - **Taking.** [`Gate::take`] joins the queue at once, so takes are served
//!   in the order they were made, whatever the order of their polls. A take
//!   is granted when it is made only while the count is below the limit and
//!   no take made before it waits.
//! - **Freeing.** A slot dropped, or a granted take dropped before its poll,
//!   goes straight to the take that has waited longest, so the count doesn't
//!   change.
//! - **Limits.** [`Gate::set_limit`] grants waiting takes while the count is
//!   below the new limit. A lowered limit grants nothing until the count
//!   falls below it.
//! - **Closing.** [`Gate::close`] gives every take, waiting or made later,
//!   [`Closed`]. Slots already held still free.
//!
//! It is built on a std mutex, held only to change the count and the queue,
//! never across an `.await`. Wakers are woken after it is released.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

/// The gate's queue and count.
pub(crate) struct Gate {
    state: Mutex<State>,
}

/// What a take gives once its gate is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Closed;

struct State {
    /// The most slots that may be held, the server's limit.
    limit: usize,
    /// Slots held, and takes granted but not yet polled.
    open: usize,
    closed: bool,
    /// The ID of the next take.
    next: u64,
    /// Takes not yet done, in the order they were made, so in ID order.
    queue: VecDeque<Entry>,
    /// When `open` last fell to 0, or when the gate was made. `None` while a
    /// slot is held.
    idle_since: Option<Instant>,
}

struct Entry {
    id: u64,
    /// Counted in `open`: the take gets its slot when it is next polled.
    granted: bool,
    /// The waker of the take's last `Pending` poll.
    waker: Option<Waker>,
}

impl Gate {
    /// A gate that lets `limit` slots be held at once.
    pub(crate) fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                limit,
                open: 0,
                closed: false,
                next: 0,
                queue: VecDeque::new(),
                idle_since: Some(Instant::now()),
            }),
        })
    }

    /// Joins the queue, now. The take is granted at once if the count is
    /// below the limit and no earlier take waits.
    pub(crate) fn take(self: &Arc<Self>) -> Take {
        let mut state = self.lock();
        let id = state.next;
        state.next += 1;
        state.queue.push_back(Entry {
            id,
            granted: false,
            waker: None,
        });
        // It is granted now if it can be. No waker is due: no take made
        // before it waits, or it couldn't be granted.
        let woken = state.admit();
        drop(state);
        wake(woken);
        Take {
            gate: self.clone(),
            id: Some(id),
        }
    }

    /// Sets the limit, and grants waiting takes while the count is below
    /// it.
    pub(crate) fn set_limit(&self, limit: usize) {
        let mut state = self.lock();
        if state.limit == limit {
            return;
        }
        state.limit = limit;
        let woken = state.admit();
        drop(state);
        wake(woken);
    }

    /// Closes the gate: every take, waiting or made later, gives [`Closed`].
    pub(crate) fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        let woken: Vec<Waker> = state
            .queue
            .iter_mut()
            .filter_map(|entry| entry.waker.take())
            .collect();
        drop(state);
        wake(woken);
    }

    /// The slots held, with the takes granted but not yet polled.
    #[cfg(test)]
    pub(crate) fn open(&self) -> usize {
        self.lock().open
    }

    /// When the count last fell to 0, or when the gate was made, or `None`
    /// while a slot is held.
    pub(crate) fn idle_since(&self) -> Option<Instant> {
        self.lock().idle_since
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl State {
    /// Grants the takes that have waited longest while the count is below
    /// the limit, and gives their wakers. A closed gate grants nothing.
    fn admit(&mut self) -> Vec<Waker> {
        let mut woken = Vec::new();
        if !self.closed {
            for entry in self.queue.iter_mut().filter(|entry| !entry.granted) {
                if self.open >= self.limit {
                    break;
                }
                entry.granted = true;
                self.open += 1;
                woken.extend(entry.waker.take());
            }
        }
        self.idle_since = match (self.open, self.idle_since) {
            (0, None) => Some(Instant::now()),
            (0, since) => since,
            _ => None,
        };
        woken
    }

    /// Frees one counted slot, which goes to the take that has waited
    /// longest, and gives that take's waker.
    fn release(&mut self) -> Vec<Waker> {
        self.open -= 1;
        self.admit()
    }

    /// Takes the entry of the take `id` out of the queue.
    fn remove(&mut self, id: u64) -> Option<Entry> {
        let at = self
            .queue
            .binary_search_by_key(&id, |entry| entry.id)
            .ok()?;
        self.queue.remove(at)
    }
}

fn wake(wakers: Vec<Waker>) {
    for waker in wakers {
        waker.wake();
    }
}

/// A place in a gate's queue, from [`Gate::take`]. It gives a [`Slot`] once
/// granted, or [`Closed`]. Dropped before then, it leaves the queue, and a
/// slot it was granted goes to the next take.
pub(crate) struct Take {
    gate: Arc<Gate>,
    /// `None` once the take is done.
    id: Option<u64>,
}

impl Future for Take {
    type Output = Result<Slot, Closed>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<Slot, Closed>> {
        let Some(id) = self.id else {
            return Poll::Ready(Err(Closed));
        };
        let mut state = self.gate.lock();
        if state.closed {
            // A closed gate grants nothing, so a freed slot wakes no one.
            if state.remove(id).is_some_and(|entry| entry.granted) {
                state.release();
            }
            drop(state);
            self.id = None;
            return Poll::Ready(Err(Closed));
        }
        let at = state
            .queue
            .binary_search_by_key(&id, |entry| entry.id)
            .expect("a waiting take is in its gate's queue");
        let entry = &mut state.queue[at];
        if entry.granted {
            state.queue.remove(at);
            drop(state);
            self.id = None;
            return Poll::Ready(Ok(Slot {
                gate: self.gate.clone(),
            }));
        }
        match &mut entry.waker {
            Some(waker) => waker.clone_from(cx.waker()),
            none => *none = Some(cx.waker().clone()),
        }
        Poll::Pending
    }
}

impl Drop for Take {
    fn drop(&mut self) {
        let Some(id) = self.id else {
            return;
        };
        let mut state = self.gate.lock();
        // A slot it was granted goes to the next take, as a held slot would.
        let woken = if state.remove(id).is_some_and(|entry| entry.granted) {
            state.release()
        } else {
            Vec::new()
        };
        drop(state);
        wake(woken);
    }
}

/// One of a gate's slots, held for one stream. Dropping it frees the slot
/// for the take that has waited longest.
pub(crate) struct Slot {
    gate: Arc<Gate>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let woken = self.gate.lock().release();
        wake(woken);
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll, Wake, Waker};
    use std::time::Instant;

    use super::*;

    fn poll_once<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
        Pin::new(future).poll(&mut Context::from_waker(Waker::noop()))
    }

    /// The slot `take` gives at once.
    fn slot(take: &mut Take) -> Slot {
        match poll_once(take) {
            Poll::Ready(Ok(slot)) => slot,
            Poll::Ready(Err(Closed)) => panic!("the gate is closed"),
            Poll::Pending => panic!("the take waits"),
        }
    }

    #[test]
    fn a_slot_is_free_below_the_limit() {
        let gate = Gate::new(2);
        let _one = slot(&mut gate.take());
        let _two = slot(&mut gate.take());
        assert!(poll_once(&mut gate.take()).is_pending());
        assert_eq!(gate.open(), 2);
    }

    #[test]
    fn a_freed_slot_goes_to_the_first_waiter() {
        let gate = Gate::new(1);
        let held = slot(&mut gate.take());
        let (mut w1, mut w2) = (gate.take(), gate.take());
        assert!(poll_once(&mut w1).is_pending());
        assert!(poll_once(&mut w2).is_pending());
        drop(held);
        assert_eq!(gate.open(), 1, "the slot went straight to w1");
        let _s1 = slot(&mut w1);
        assert!(poll_once(&mut w2).is_pending());
    }

    #[test]
    fn a_new_taker_never_jumps_the_queue() {
        let gate = Gate::new(1);
        let held = slot(&mut gate.take());
        let mut w1 = gate.take();
        assert!(poll_once(&mut w1).is_pending());
        drop(held);
        let mut late = gate.take();
        assert!(poll_once(&mut late).is_pending(), "w1 has the slot");
        let _s1 = slot(&mut w1);
        assert!(poll_once(&mut late).is_pending());
    }

    /// The pool relies on this: it makes the takes of the hops that waited
    /// on a connect in the order they waited.
    #[test]
    fn takes_queue_in_the_order_they_were_made() {
        let gate = Gate::new(1);
        let held = slot(&mut gate.take());
        let (mut w1, mut w2) = (gate.take(), gate.take());
        assert!(poll_once(&mut w2).is_pending());
        drop(held);
        assert!(poll_once(&mut w2).is_pending(), "w1 was made first");
        let _s1 = slot(&mut w1);
    }

    #[test]
    fn a_waiter_dropped_after_its_grant_passes_the_slot_on() {
        let gate = Gate::new(1);
        let held = slot(&mut gate.take());
        let (mut w1, mut w2) = (gate.take(), gate.take());
        assert!(poll_once(&mut w1).is_pending());
        assert!(poll_once(&mut w2).is_pending());
        drop(held);
        drop(w1);
        let _s2 = slot(&mut w2);
        assert_eq!(gate.open(), 1);
    }

    #[test]
    fn a_dropped_waiter_leaves_the_queue() {
        let gate = Gate::new(1);
        let held = slot(&mut gate.take());
        let (mut w1, mut w2) = (gate.take(), gate.take());
        assert!(poll_once(&mut w1).is_pending());
        assert!(poll_once(&mut w2).is_pending());
        drop(w1);
        drop(held);
        let _s2 = slot(&mut w2);
        assert_eq!(gate.open(), 1);
    }

    #[test]
    fn a_raised_limit_admits_waiters() {
        let gate = Gate::new(1);
        let _held = slot(&mut gate.take());
        let mut w1 = gate.take();
        assert!(poll_once(&mut w1).is_pending());
        gate.set_limit(2);
        let _s1 = slot(&mut w1);
        assert_eq!(gate.open(), 2);
    }

    #[test]
    fn a_lowered_limit_admits_no_one_until_the_count_falls_below_it() {
        let gate = Gate::new(3);
        let (a, b, c) = (
            slot(&mut gate.take()),
            slot(&mut gate.take()),
            slot(&mut gate.take()),
        );
        let mut waiter = gate.take();
        assert!(poll_once(&mut waiter).is_pending());
        gate.set_limit(1);
        drop((a, b));
        assert!(poll_once(&mut waiter).is_pending());
        drop(c);
        let _slot = slot(&mut waiter);
        assert_eq!(gate.open(), 1);
    }

    /// A waker that records that it was woken.
    #[derive(Default)]
    struct Woken(AtomicBool);

    impl Wake for Woken {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn closing_wakes_every_waiter_with_closed() {
        let gate = Gate::new(2);
        let held = slot(&mut gate.take());
        // Granted when made, and not yet polled.
        let mut granted = gate.take();
        let woken = Arc::new(Woken::default());
        let mut queued = gate.take();
        let polled =
            Pin::new(&mut queued).poll(&mut Context::from_waker(&Waker::from(woken.clone())));
        assert!(polled.is_pending());

        gate.close();
        assert!(woken.0.load(Ordering::SeqCst), "closing wakes the waiter");
        for take in [&mut granted, &mut queued, &mut gate.take()] {
            assert!(matches!(poll_once(take), Poll::Ready(Err(Closed))));
        }
        assert_eq!(gate.open(), 1, "the held slot");
        drop(held);
        assert_eq!(gate.open(), 0);
    }

    #[test]
    fn the_gate_is_idle_from_the_end_of_its_last_slot() {
        let gate = Gate::new(2);
        assert!(gate.idle_since().is_some(), "a new gate is idle");
        let one = slot(&mut gate.take());
        let two = slot(&mut gate.take());
        assert_eq!(gate.idle_since(), None);
        drop(one);
        assert_eq!(gate.idle_since(), None, "a slot is still held");
        let before = Instant::now();
        drop(two);
        assert!(gate.idle_since().is_some_and(|since| since >= before));
    }
}
