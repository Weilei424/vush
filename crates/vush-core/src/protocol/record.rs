//! What a hop, an attempt and a step leave behind: timings, sizes and how it
//! ended.
//!
//! Records hold numbers and labels only, never text. Phases 5 and 6 read them.

use std::time::Duration;

use super::{FailureKind, FailureStage};

/// How long a hop took. Only the parts an executor can measure are required.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HopTiming {
    /// From the start of `send` until the response head arrived. `None` if it
    /// never did.
    pub to_headers: Option<Duration>,
    /// From the start of `send` until the body was read or the hop failed.
    pub total: Duration,
    /// Present only when this hop opened a new connection and the executor
    /// measured it. With a shared pool, a new connection's setup is reported
    /// on the first hop that used it, which may not be the hop that triggered
    /// it.
    pub setup: Option<ConnectionSetup>,
}

/// The time spent setting up a new connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionSetup {
    /// `None` when no lookup was needed, as for an IP address, or it wasn't
    /// measured.
    pub dns: Option<Duration>,
    /// The time to open the connection.
    pub connect: Duration,
    /// `None` for plain HTTP.
    pub tls: Option<Duration>,
}

/// Tells connections apart within a run. The executor assigns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionId(pub u64);

/// Body bytes for one hop. Headers, framing and TLS are not counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BodySizes {
    /// Request body bytes after encoding. 0 when there was no body or nothing
    /// was sent.
    pub sent: u64,
    /// Response body bytes read. At most the size limit when the body was cut.
    pub received: u64,
}

/// How a hop or an attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A response arrived, whatever its status. `truncated` is set when the
    /// body was cut at the size limit.
    Response {
        /// The HTTP status code.
        status: u16,
        /// Whether the body was cut at the size limit.
        truncated: bool,
    },
    /// No complete response arrived, or the attempt ended in failure.
    Failed {
        /// What went wrong.
        kind: FailureKind,
        /// Where it went wrong.
        stage: FailureStage,
    },
}

/// One request sent and what came back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HopRecord {
    /// How long the hop took.
    pub timing: HopTiming,
    /// The connection that carried the hop, if the executor can tell.
    pub connection: Option<ConnectionId>,
    /// The body bytes sent and received.
    pub sizes: BodySizes,
    /// How the hop ended.
    pub outcome: Outcome,
}

/// One try at a request step: its first hop, plus one per redirect followed.
///
/// An attempt always has at least one hop. It can also end in a failure after
/// its last hop, such as a redirect that can't be followed, because nothing is
/// sent then and so there is no hop to carry it. Judge an attempt by
/// [`outcome`](Self::outcome), never by its last hop alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptRecord {
    hops: Vec<HopRecord>,
    ending: Option<(FailureKind, FailureStage)>,
}

impl AttemptRecord {
    /// Starts an attempt with its first hop.
    pub fn new(first: HopRecord) -> Self {
        Self {
            hops: vec![first],
            ending: None,
        }
    }

    /// Adds the next hop. It is a bug to add one after the attempt has ended.
    pub fn push(&mut self, hop: HopRecord) {
        debug_assert!(self.ending.is_none(), "an ended attempt takes no more hops");
        self.hops.push(hop);
    }

    /// Ends the attempt with a failure that no hop carries. It is a bug to end
    /// an attempt twice.
    pub fn end_with(&mut self, kind: FailureKind, stage: FailureStage) {
        debug_assert!(self.ending.is_none(), "an attempt ends only once");
        self.ending = Some((kind, stage));
    }

    /// The hops, in the order they were sent. Never empty.
    pub fn hops(&self) -> &[HopRecord] {
        &self.hops
    }

    /// How the attempt ended: its ending if it has one, otherwise the outcome
    /// of its last hop.
    pub fn outcome(&self) -> Outcome {
        match self.ending {
            Some((kind, stage)) => Outcome::Failed { kind, stage },
            None => {
                self.hops
                    .last()
                    .expect("an attempt has at least one hop")
                    .outcome
            }
        }
    }
}

/// All the attempts a request step made, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRecord {
    attempts: Vec<AttemptRecord>,
}

impl StepRecord {
    /// Starts a step with its first attempt.
    pub fn new(first: AttemptRecord) -> Self {
        Self {
            attempts: vec![first],
        }
    }

    /// Adds the next attempt.
    pub fn push(&mut self, attempt: AttemptRecord) {
        self.attempts.push(attempt);
    }

    /// The attempts, in the order they were made. Never empty.
    pub fn attempts(&self) -> &[AttemptRecord] {
        &self.attempts
    }
}
