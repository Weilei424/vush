//! The protocol boundary, used the way an outside crate uses it.

use std::collections::HashSet;

use vush_core::protocol::{Failure, FailureKind, FailureStage};

const _: () = {
    const fn assert_shareable<T: Send + Sync + 'static>() {}
    assert_shareable::<FailureKind>();
    assert_shareable::<FailureStage>();
    assert_shareable::<Failure>();
};

/// Each kind's place in `FailureKind::ALL`. The match has no wildcard, so a
/// new kind won't compile until it is placed here and in `ALL`.
fn kind_position(kind: FailureKind) -> usize {
    match kind {
        FailureKind::InvalidRequest => 0,
        FailureKind::Dns => 1,
        FailureKind::Connect => 2,
        FailureKind::Tls => 3,
        FailureKind::Timeout => 4,
        FailureKind::ConnectionClosed => 5,
        FailureKind::Protocol => 6,
        FailureKind::InvalidRedirect => 7,
        FailureKind::TooManyRedirects => 8,
        FailureKind::Other => 9,
    }
}

fn stage_position(stage: FailureStage) -> usize {
    match stage {
        FailureStage::Prepare => 0,
        FailureStage::Connect => 1,
        FailureStage::Headers => 2,
        FailureStage::Body => 3,
    }
}

#[test]
fn failure_kind_labels_are_pinned() {
    let labels: Vec<&str> = FailureKind::ALL.iter().map(|kind| kind.label()).collect();
    assert_eq!(
        labels,
        [
            "invalid_request",
            "dns",
            "connect",
            "tls",
            "timeout",
            "connection_closed",
            "protocol",
            "invalid_redirect",
            "too_many_redirects",
            "other",
        ]
    );
    for (position, kind) in FailureKind::ALL.into_iter().enumerate() {
        assert_eq!(kind_position(kind), position, "{kind:?}");
    }
}

#[test]
fn failure_stage_labels_are_pinned() {
    let labels: Vec<&str> = FailureStage::ALL
        .iter()
        .map(|stage| stage.label())
        .collect();
    assert_eq!(labels, ["prepare", "connect", "headers", "body"]);
    for (position, stage) in FailureStage::ALL.into_iter().enumerate() {
        assert_eq!(stage_position(stage), position, "{stage:?}");
    }
}

fn assert_snake_case(label: &str) {
    assert!(!label.is_empty(), "{label}");
    assert!(
        !label.starts_with('_') && !label.ends_with('_') && !label.contains("__"),
        "{label}"
    );
    assert!(
        label.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
        "{label}"
    );
}

#[test]
fn labels_are_unique_snake_case_and_match_display() {
    let kinds: Vec<(&str, String)> = FailureKind::ALL
        .iter()
        .map(|k| (k.label(), k.to_string()))
        .collect();
    let stages: Vec<(&str, String)> = FailureStage::ALL
        .iter()
        .map(|s| (s.label(), s.to_string()))
        .collect();
    for list in [&kinds, &stages] {
        for (label, shown) in list {
            assert_eq!(label, shown);
            assert_snake_case(label);
        }
        let unique: HashSet<&str> = list.iter().map(|(label, _)| *label).collect();
        assert_eq!(unique.len(), list.len());
    }
}

#[test]
fn a_failure_displays_as_one_line() {
    let failure = Failure {
        kind: FailureKind::Timeout,
        stage: FailureStage::Body,
        message: "no data in time",
    };
    assert_eq!(failure.to_string(), "timeout in body: no data in time");
    let error: &dyn std::error::Error = &failure;
    assert!(error.source().is_none());
}
