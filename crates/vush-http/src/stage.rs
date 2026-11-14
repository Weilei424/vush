//! Which stage a hop is in, so a timeout can report where it struck.
//!
//! `exchange` owns each hop's marker. The marker starts at `connect` once
//! `prepare` is done, moves to `headers` when hyper-util assigns the hop a
//! connection, new or pooled, and moves to `body` when the head arrives.
//!
//! A marker only moves forward, in the order `prepare`, `connect`, `headers`,
//! `body`, so a move that comes late can't take a hop back to an earlier
//! stage.

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
}
