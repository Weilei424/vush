//! Which stage a hop is in, so a timeout can report where it struck.
//!
//! `send` sets [`MARKER`] around the hop's future. Code that runs inside that
//! task, such as the connector, moves it with [`mark`].
//!
//! A marker only moves forward, in the order `prepare`, `connect`, `headers`,
//! `body`. So a late move, such as the connector's when hyper-util retries a
//! request on a new connection, can't take a hop back to an earlier stage.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use vush_core::protocol::FailureStage;

/// A hop's current stage, shared between the hop and the code it drives.
/// Clones share one stage, and it only moves forward.
#[derive(Clone)]
pub(crate) struct StageMarker(Arc<AtomicU8>);

impl StageMarker {
    /// A marker in `prepare`.
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicU8::new(encode(FailureStage::Prepare))))
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

tokio::task_local! {
    /// The marker of the hop whose task is running.
    pub(crate) static MARKER: StageMarker;
}

/// Moves the running hop's marker forward to `stage`. Outside a hop's task,
/// as when hyper-util finishes a connect in the background, it does nothing.
pub(crate) fn mark(stage: FailureStage) {
    // An error only means no marker is in scope.
    let _ = MARKER.try_with(|marker| marker.set(stage));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marker_starts_in_prepare_and_keeps_each_stage() {
        let marker = StageMarker::new();
        assert_eq!(marker.get(), FailureStage::Prepare);
        let clone = marker.clone();
        for stage in FailureStage::ALL {
            clone.set(stage);
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
    fn mark_moves_the_marker_only_in_its_scope() {
        let marker = StageMarker::new();
        mark(FailureStage::Body);
        MARKER.sync_scope(marker.clone(), || mark(FailureStage::Headers));
        assert_eq!(marker.get(), FailureStage::Headers);
        mark(FailureStage::Body);
        assert_eq!(marker.get(), FailureStage::Headers);
    }
}
