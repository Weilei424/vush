//! Thresholds: pass/fail checks on run metrics.

use std::time::Duration;

use super::value::{Rate, Ratio};

/// A percentile of a duration metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PercentileStat {
    P50,
    P90,
    P95,
    P99,
}

/// How a metric is compared with a bound's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundOp {
    Lt,
    Lte,
    Gt,
    Gte,
}

/// A comparison against a typed value, so a duration metric can't hold a
/// percentage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound<T> {
    pub op: BoundOp,
    pub value: T,
}

/// One check, with one variant per metric.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThresholdCheck {
    HttpRequestDuration {
        stat: PercentileStat,
        bound: Bound<Duration>,
    },
    ErrorRate(Bound<Ratio>),
    RequestRate(Bound<Rate>),
    IterationRate(Bound<Rate>),
    DroppedWorkRatio(Bound<Ratio>),
}

/// A threshold, for the whole run or for one scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Threshold {
    /// Limits the check to one scenario by name.
    pub scenario: Option<String>,
    pub check: ThresholdCheck,
}
