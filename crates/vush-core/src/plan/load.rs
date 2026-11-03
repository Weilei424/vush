//! Load models: how a scenario's work is generated over time.

use std::num::{NonZeroU32, NonZeroU64};
use std::time::Duration;

use super::value::Rate;

/// How a scenario generates load. The three models are distinct.
#[derive(Debug, Clone, PartialEq)]
pub enum LoadModel {
    /// A set of virtual users, each looping over the steps.
    ClosedVus(ClosedVusLoad),
    /// Iterations start at a rate, whatever the users are doing.
    IterationRate {
        schedule: RateSchedule,
        max_vus: NonZeroU32,
        queue: QueuePolicy,
    },
    /// Physical HTTP request starts are paced at a rate.
    RequestRate {
        schedule: RateSchedule,
        /// The users generating the requests.
        vus: NonZeroU32,
    },
}

/// A closed-model load: the number of users is set, not the rate.
#[derive(Debug, Clone, PartialEq)]
pub enum ClosedVusLoad {
    Fixed {
        vus: NonZeroU32,
        stop: ClosedStop,
    },
    Ramping {
        /// The user count at time zero.
        start: u32,
        stages: Vec<Stage<u32>>,
    },
}

/// When a fixed closed-model load stops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClosedStop {
    Duration(Duration),
    /// Iterations each user runs before stopping.
    IterationsPerVu(NonZeroU64),
}

/// A rate over time, for the iteration-rate and request-rate models.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateSchedule {
    Fixed {
        rate: Rate,
        duration: Duration,
    },
    Ramping {
        /// The rate at time zero.
        start: Rate,
        stages: Vec<Stage<Rate>>,
    },
}

/// One ramp stage: move to `to` over `over`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage<T> {
    pub to: T,
    pub over: Duration,
}

/// The bounded queue of iteration arrivals that no user has picked up yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuePolicy {
    /// A full queue drops the newest arrival.
    pub capacity: NonZeroU32,
    /// Work waiting longer than this expires and never runs late.
    pub max_wait: Duration,
}

impl Default for QueuePolicy {
    fn default() -> Self {
        Self {
            capacity: NonZeroU32::new(1000).expect("1000 is not zero"),
            max_wait: Duration::from_secs(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_policy_default_matches_schema() {
        assert_eq!(
            QueuePolicy::default(),
            QueuePolicy {
                capacity: NonZeroU32::new(1000).unwrap(),
                max_wait: Duration::from_secs(1)
            }
        );
    }
}
