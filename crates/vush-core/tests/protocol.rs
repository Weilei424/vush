//! The protocol boundary, used the way an outside crate uses it.

use std::collections::HashSet;
use std::time::Duration;

use vush_core::protocol::{
    AttemptRecord, BodySizes, ConnectionId, ConnectionSetup, Failure, FailureKind, FailureStage,
    HopRecord, HopTiming, Outcome, StepRecord,
};

const _: () = {
    const fn assert_shareable<T: Send + Sync + 'static>() {}
    assert_shareable::<FailureKind>();
    assert_shareable::<FailureStage>();
    assert_shareable::<Failure>();
    assert_shareable::<HopTiming>();
    assert_shareable::<ConnectionSetup>();
    assert_shareable::<ConnectionId>();
    assert_shareable::<BodySizes>();
    assert_shareable::<Outcome>();
    assert_shareable::<HopRecord>();
    assert_shareable::<AttemptRecord>();
    assert_shareable::<StepRecord>();
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

fn hop_record(outcome: Outcome) -> HopRecord {
    HopRecord {
        timing: HopTiming {
            to_headers: Some(Duration::from_millis(3)),
            total: Duration::from_millis(5),
            setup: Some(ConnectionSetup {
                dns: None,
                connect: Duration::from_millis(1),
                tls: None,
            }),
        },
        connection: Some(ConnectionId(1)),
        sizes: BodySizes::default(),
        outcome,
    }
}

fn answered(status: u16) -> Outcome {
    Outcome::Response {
        status,
        truncated: false,
    }
}

#[test]
fn records_count_hops_attempts_and_steps_separately() {
    let mut first = AttemptRecord::new(hop_record(answered(302)));
    first.push(hop_record(answered(503)));
    let mut second = AttemptRecord::new(hop_record(answered(302)));
    second.push(hop_record(answered(200)));
    let mut step = StepRecord::new(first);
    step.push(second);

    assert_eq!(step.attempts().len(), 2);
    let hops: usize = step
        .attempts()
        .iter()
        .map(|attempt| attempt.hops().len())
        .sum();
    assert_eq!(hops, 4);
    let outcomes: Vec<Outcome> = step.attempts().iter().map(AttemptRecord::outcome).collect();
    assert_eq!(outcomes, [answered(503), answered(200)]);
}

#[test]
fn new_records_are_never_empty() {
    let attempt = AttemptRecord::new(hop_record(answered(200)));
    assert_eq!(attempt.hops().len(), 1);
    assert_eq!(StepRecord::new(attempt).attempts().len(), 1);
}

#[test]
fn an_ending_after_the_last_hop_is_the_attempts_outcome() {
    for kind in [FailureKind::InvalidRedirect, FailureKind::TooManyRedirects] {
        let mut attempt = AttemptRecord::new(hop_record(answered(302)));
        attempt.end_with(kind, FailureStage::Prepare);
        assert_eq!(attempt.hops().len(), 1);
        assert_eq!(
            attempt.outcome(),
            Outcome::Failed {
                kind,
                stage: FailureStage::Prepare
            }
        );
    }
}

#[test]
fn without_an_ending_an_attempt_reports_its_last_hop() {
    let refused = Outcome::Failed {
        kind: FailureKind::Connect,
        stage: FailureStage::Connect,
    };
    let mut attempt = AttemptRecord::new(hop_record(answered(302)));
    attempt.push(hop_record(refused));
    assert_eq!(attempt.outcome(), refused);
}

#[test]
#[cfg(debug_assertions)]
#[should_panic]
fn pushing_after_an_ending_is_a_bug() {
    let mut attempt = AttemptRecord::new(hop_record(answered(302)));
    attempt.end_with(FailureKind::TooManyRedirects, FailureStage::Prepare);
    attempt.push(hop_record(answered(200)));
}

#[test]
#[cfg(debug_assertions)]
#[should_panic]
fn ending_twice_is_a_bug() {
    let mut attempt = AttemptRecord::new(hop_record(answered(302)));
    attempt.end_with(FailureKind::InvalidRedirect, FailureStage::Prepare);
    attempt.end_with(FailureKind::TooManyRedirects, FailureStage::Prepare);
}
