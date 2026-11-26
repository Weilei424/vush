//! Which stage a hop is in, so a timeout can report where it struck.
//!
//! `exchange` owns each hop's marker. The marker starts at `connect` once
//! `prepare` is done, moves to `headers` when hyper gives the hop's request to
//! a connection (see [`Unwritten`]), and moves to `body` when the head
//! arrives.
//!
//! A marker only moves forward, in the order `prepare`, `connect`, `headers`,
//! `body`, so a move that comes late can't take a hop back to an earlier
//! stage.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

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

/// A mark a hop's request carries in its extensions. hyper drops a request's
/// extensions when it gives the request to a connection: under HTTP/1.1 once
/// it has written the head into the connection's buffer, and under HTTP/2
/// once h2 takes the stream, which h2 may hold until the server allows
/// another. A request hyper hands back, for hyper-util to send on another
/// connection, keeps them. When the last copy of the mark drops, the hop's
/// marker moves to `headers`.
///
/// hyper-util also drops a request it can't send, when a connect fails,
/// inside the hop's poll that then gives the failure. A connect failure is
/// placed in `connect` by its cause, and `settle` keeps it there.
#[derive(Clone)]
pub(crate) struct Unwritten {
    _mark: Arc<OnWritten>,
}

struct OnWritten(Arc<StageMarker>);

impl Unwritten {
    /// A mark that moves `marker` to `headers` when its last copy drops.
    pub(crate) fn new(marker: Arc<StageMarker>) -> Self {
        Self {
            _mark: Arc::new(OnWritten(marker)),
        }
    }
}

/// Moves the marker once the request has left hyper's hands.
impl Drop for OnWritten {
    fn drop(&mut self) {
        self.0.set(FailureStage::Headers);
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
}
