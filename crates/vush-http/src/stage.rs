//! Which stage a hop is in, so a timeout can report where it struck.
//!
//! `exchange` owns each hop's marker. The marker starts at `connect` once
//! `prepare` is done, and moves to `headers` when the pool hands the hop's
//! request to a connection: under HTTP/1.1 through the request's mark when
//! hyper writes the head (see [`Unwritten`]), and under HTTP/2 when the pool
//! hands h2 the request, which needs no mark. It moves to `body` when the
//! head arrives.
//!
//! A marker only moves forward, in the order `prepare`, `connect`, `headers`,
//! `body`, so a move that comes late can't take a hop back to an earlier
//! stage.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use vush_core::protocol::FailureStage;

/// A hop's current stage. It only moves forward.
pub(crate) struct StageMarker(AtomicU8);

impl StageMarker {
    /// A marker in `prepare`.
    pub(crate) fn new() -> Self {
        Self(AtomicU8::new(encode(FailureStage::Prepare)))
    }

    /// Moves to `stage`, unless the marker is already past it.
    pub(crate) fn set(&self, stage: FailureStage) {
        self.0.fetch_max(encode(stage), Ordering::Relaxed);
    }

    pub(crate) fn get(&self) -> FailureStage {
        match self.0.load(Ordering::Relaxed) {
            0 => FailureStage::Prepare,
            1 => FailureStage::Connect,
            2 => FailureStage::Headers,
            _ => FailureStage::Body,
        }
    }
}

/// A mark an HTTP/1.1 request carries in its extensions. It serves HTTP/1.1
/// only: under HTTP/2 the pool moves the marker itself. hyper drops a
/// request's extensions once it has written the head into the connection's
/// buffer. When the last copy of the mark drops, the hop's marker moves to
/// `headers`, unless the mark was disarmed.
///
/// A request hyper hands back unsent keeps its extensions. The pool disarms
/// its mark before it drops it, so the hop stays in `connect`. The next
/// HTTP/1.1 connection the hop tries is handed a copy with a mark of its
/// own.
#[derive(Clone)]
pub(crate) struct Unwritten {
    mark: Arc<OnWritten>,
}

struct OnWritten {
    marker: Arc<StageMarker>,
    /// Cleared by [`Unwritten::disarm`].
    armed: AtomicBool,
}

impl Unwritten {
    /// A mark that moves `marker` to `headers` when its last copy drops.
    pub(crate) fn new(marker: Arc<StageMarker>) -> Self {
        Self {
            mark: Arc::new(OnWritten {
                marker,
                armed: AtomicBool::new(true),
            }),
        }
    }

    /// Makes the mark leave its marker alone when its last copy drops, for a
    /// request that drops without having reached a connection.
    pub(crate) fn disarm(&self) {
        self.mark.armed.store(false, Ordering::Relaxed);
    }
}

/// Moves the marker once the request has left hyper's hands.
impl Drop for OnWritten {
    fn drop(&mut self) {
        if *self.armed.get_mut() {
            self.marker.set(FailureStage::Headers);
        }
    }
}

/// Stages in the order a hop goes through them.
fn encode(stage: FailureStage) -> u8 {
    match stage {
        FailureStage::Prepare => 0,
        FailureStage::Connect => 1,
        FailureStage::Headers => 2,
        FailureStage::Body => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marker_starts_in_prepare_and_keeps_each_stage() {
        let marker = StageMarker::new();
        assert_eq!(marker.get(), FailureStage::Prepare);
        for stage in FailureStage::ALL {
            marker.set(stage);
            assert_eq!(marker.get(), stage);
        }
    }

    #[test]
    fn a_marker_never_moves_backwards() {
        let marker = StageMarker::new();
        marker.set(FailureStage::Headers);
        for earlier in [FailureStage::Connect, FailureStage::Prepare] {
            marker.set(earlier);
            assert_eq!(marker.get(), FailureStage::Headers, "{earlier:?}");
        }
        marker.set(FailureStage::Body);
        assert_eq!(marker.get(), FailureStage::Body);
    }

    #[test]
    fn a_dropped_mark_moves_its_marker_to_headers() {
        let marker = Arc::new(StageMarker::new());
        marker.set(FailureStage::Connect);
        let mark = Unwritten::new(marker.clone());
        let copy = mark.clone();
        drop(mark);
        assert_eq!(marker.get(), FailureStage::Connect);
        drop(copy);
        assert_eq!(marker.get(), FailureStage::Headers);

        let marker = Arc::new(StageMarker::new());
        let mark = Unwritten::new(marker.clone());
        marker.set(FailureStage::Body);
        drop(mark);
        assert_eq!(marker.get(), FailureStage::Body);
    }

    #[test]
    fn a_disarmed_mark_leaves_its_marker_where_it_is() {
        let marker = Arc::new(StageMarker::new());
        marker.set(FailureStage::Connect);
        let mark = Unwritten::new(marker.clone());
        let copy = mark.clone();
        copy.disarm();
        drop(copy);
        drop(mark);
        assert_eq!(marker.get(), FailureStage::Connect);
    }
}
