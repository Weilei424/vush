//! Which stage a hop is in, so a timeout can report where it struck.
//!
//! `send` sets [`MARKER`] around the hop's future. Code that runs inside that
//! task, such as the connector, moves it with [`mark`].

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use vush_core::protocol::FailureStage;

/// A hop's current stage, shared between the hop and the code it drives.
/// Clones share one stage.
#[derive(Clone)]
pub(crate) struct StageMarker(Arc<AtomicU8>);

impl StageMarker {
    /// A marker in `prepare`.
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicU8::new(encode(FailureStage::Prepare))))
    }

    pub(crate) fn set(&self, stage: FailureStage) {
        self.0.store(encode(stage), Ordering::Relaxed);
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

/// Moves the running hop's marker to `stage`. Outside a hop's task, as when
/// hyper-util finishes a connect in the background, it does nothing.
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
    fn mark_moves_the_marker_only_in_its_scope() {
        let marker = StageMarker::new();
        mark(FailureStage::Body);
        MARKER.sync_scope(marker.clone(), || mark(FailureStage::Headers));
        assert_eq!(marker.get(), FailureStage::Headers);
        mark(FailureStage::Body);
        assert_eq!(marker.get(), FailureStage::Headers);
    }
}
