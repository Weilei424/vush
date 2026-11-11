//! The boundary between core and a protocol executor.
//!
//! A request step makes attempts, and `retry.attempts` caps them. An attempt
//! makes hops: its first request, plus one per redirect followed, and
//! `http.redirects` caps those.
//!
//! An executor's `send` makes exactly one hop. Core's driver owns both loops,
//! so it sees every hop before it starts. A rejected redirect, or a chain
//! longer than `http.redirects`, ends the attempt without a hop.
//!
//! The types here are protocol-neutral. The HTTP request and response types
//! and the executor trait are in [`http`].
//!
//! A failure carries a fixed message, and the `Debug` output of resolved
//! requests and responses never shows URLs, header values or bodies.

mod failure;
pub mod http;
mod json;
mod record;

pub use failure::{Failure, FailureKind, FailureStage};
pub use json::JsonValue;
pub use record::{
    AttemptRecord, BodySizes, ConnectionId, ConnectionSetup, HopRecord, HopTiming, Outcome,
    StepRecord,
};
