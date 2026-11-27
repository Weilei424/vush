//! A one-shot handoff from one side to a hop that waits for it.
//!
//! The pool gives a waiting hop its connection through one: another hop
//! returning one, or the connect the hop started. It is built on a std
//! mutex and the hop's waker. The mutex is held only to move the value or
//! the waker, never across an `.await`, and a value is dropped only after
//! it is released.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};

/// A giver and a taker that share one slot.
pub(crate) fn handoff<T>() -> (Giver<T>, Taker<T>) {
    let slot = Arc::new(Mutex::new(Slot {
        value: None,
        waker: None,
        giver_gone: false,
        taker_gone: false,
    }));
    (Giver { slot: slot.clone() }, Taker { slot })
}

struct Slot<T> {
    /// What was given and not yet taken.
    value: Option<T>,
    /// The taker's waker, from its last `Pending` poll.
    waker: Option<Waker>,
    /// Set when the giver has given, or was dropped without giving.
    giver_gone: bool,
    /// Set when the taker was closed or dropped.
    taker_gone: bool,
}

fn lock<T>(slot: &Mutex<Slot<T>>) -> MutexGuard<'_, Slot<T>> {
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Gives one value to its [`Taker`].
pub(crate) struct Giver<T> {
    slot: Arc<Mutex<Slot<T>>>,
}

impl<T> Giver<T> {
    /// Gives `value` and wakes the taker. A taker that was closed or dropped
    /// can't take it, so it comes back as `Err`, for the caller to place
    /// elsewhere.
    pub(crate) fn give(self, value: T) -> Result<(), T> {
        let waker = {
            let mut slot = lock(&self.slot);
            if slot.taker_gone {
                return Err(value);
            }
            slot.value = Some(value);
            slot.giver_gone = true;
            slot.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(())
    }
}

/// Wakes the taker, which then gives `None` unless a value was given.
impl<T> Drop for Giver<T> {
    fn drop(&mut self) {
        let waker = {
            let mut slot = lock(&self.slot);
            slot.giver_gone = true;
            slot.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// Waits for its [`Giver`]'s value: `Some` once given, `None` if the giver
/// was dropped without giving.
pub(crate) struct Taker<T> {
    slot: Arc<Mutex<Slot<T>>>,
}

impl<T> Taker<T> {
    /// Stops taking. Every later [`Giver::give`] gets its value back, and a
    /// value given before is returned, so the caller can put it elsewhere
    /// and it is never lost. Polls after this give `None`.
    pub(crate) fn close(&mut self) -> Option<T> {
        let mut slot = lock(&self.slot);
        slot.taker_gone = true;
        slot.giver_gone = true;
        slot.waker = None;
        slot.value.take()
    }
}

impl<T> Future for Taker<T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut slot = lock(&self.slot);
        if let Some(value) = slot.value.take() {
            return Poll::Ready(Some(value));
        }
        if slot.giver_gone {
            return Poll::Ready(None);
        }
        match &mut slot.waker {
            Some(waker) => waker.clone_from(cx.waker()),
            none => *none = Some(cx.waker().clone()),
        }
        Poll::Pending
    }
}

/// A value given and not taken is dropped with the taker, after the slot is
/// released. Callers that must not lose it call [`Taker::close`] first.
impl<T> Drop for Taker<T> {
    fn drop(&mut self) {
        let value = {
            let mut slot = lock(&self.slot);
            slot.taker_gone = true;
            slot.waker = None;
            slot.value.take()
        };
        drop(value);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::Wake;

    use super::*;

    /// A waker that records that it was woken.
    #[derive(Default)]
    struct Woken(AtomicBool);

    impl Wake for Woken {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn poll<T>(taker: &mut Taker<T>, waker: &Waker) -> Poll<Option<T>> {
        Pin::new(taker).poll(&mut Context::from_waker(waker))
    }

    #[test]
    fn a_given_value_is_taken() {
        let (giver, mut taker) = handoff();
        let woken = Arc::new(Woken::default());
        let waker = Waker::from(woken.clone());
        assert_eq!(poll(&mut taker, &waker), Poll::Pending);
        assert_eq!(giver.give(7), Ok(()));
        assert!(woken.0.load(Ordering::SeqCst), "giving wakes the taker");
        assert_eq!(poll(&mut taker, &waker), Poll::Ready(Some(7)));
    }

    #[test]
    fn a_giver_dropped_without_giving_gives_none() {
        let (giver, mut taker) = handoff::<u8>();
        let woken = Arc::new(Woken::default());
        let waker = Waker::from(woken.clone());
        assert_eq!(poll(&mut taker, &waker), Poll::Pending);
        drop(giver);
        assert!(woken.0.load(Ordering::SeqCst), "the drop wakes the taker");
        assert_eq!(poll(&mut taker, &waker), Poll::Ready(None));
    }

    #[test]
    fn giving_to_a_dropped_taker_gives_the_value_back() {
        let (giver, taker) = handoff();
        drop(taker);
        assert_eq!(giver.give(7), Err(7));
    }

    #[test]
    fn a_closed_taker_gives_back_what_it_was_given_and_refuses_the_rest() {
        let (giver, mut taker) = handoff();
        assert_eq!(giver.give(7), Ok(()));
        assert_eq!(taker.close(), Some(7));

        let (giver, mut taker) = handoff();
        assert_eq!(taker.close(), None);
        assert_eq!(giver.give(8), Err(8));
        assert_eq!(poll(&mut taker, Waker::noop()), Poll::Ready(None));
    }
}
