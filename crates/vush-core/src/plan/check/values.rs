//! Checks on durations, rates and load models.

use std::collections::HashSet;
use std::time::Duration;

use super::super::error::{PlanError, PlanErrorKind};
use super::super::load::{ClosedStop, ClosedVusLoad, LoadModel, RateSchedule, Stage};
use super::super::step::{Condition, HttpMethod, RequestTarget, RetryPolicy, StatusMatch};
use super::super::value::{JsonNumber, MAX_DURATION, Rate};
use super::{At, push};

/// Checks a duration at `at`: it may not be above [`MAX_DURATION`], and when
/// `positive` it may not be zero.
pub(super) fn duration(value: Duration, positive: bool, at: &At<'_>, errors: &mut Vec<PlanError>) {
    if positive && value.is_zero() {
        push(errors, at, PlanErrorKind::ZeroDuration);
    } else if value > MAX_DURATION {
        push(errors, at, PlanErrorKind::DurationTooLong);
    }
}

/// Checks a positive duration that was copied from `parent` unless the file
/// set it, so a bad inherited value is reported once, where it was set.
pub(super) fn inherited(
    value: Duration,
    parent: Duration,
    at: &At<'_>,
    errors: &mut Vec<PlanError>,
) {
    if value != parent {
        duration(value, true, at, errors);
    }
}

/// Checks a request-rate budget, if there is one.
pub(super) fn rate_budget(rate: Option<Rate>, at: &At<'_>, errors: &mut Vec<PlanError>) {
    if let Some(rate) = rate {
        fixed_rate(rate, at, errors);
    }
}

fn fixed_rate(rate: Rate, at: &At<'_>, errors: &mut Vec<PlanError>) {
    if rate.count == 0 {
        push(errors, at, PlanErrorKind::ZeroRate);
    }
}

/// Checks a load model, whose own place is `at`.
pub(super) fn load(load: &LoadModel, at: &At<'_>, errors: &mut Vec<PlanError>) {
    match load {
        LoadModel::ClosedVus(ClosedVusLoad::Fixed { stop, .. }) => {
            if let ClosedStop::Duration(value) = stop {
                duration(*value, true, &at.key("duration"), errors);
            }
        }
        LoadModel::ClosedVus(ClosedVusLoad::Ramping { start, stages }) => {
            ramp(*start > 0, stages, |to| *to > 0, at, errors);
        }
        LoadModel::IterationRate {
            schedule, queue, ..
        } => {
            rate_schedule(schedule, at, errors);
            duration(
                queue.max_wait,
                false,
                &at.key("queue").key("max_wait"),
                errors,
            );
        }
        LoadModel::RequestRate { schedule, .. } => rate_schedule(schedule, at, errors),
    }
}

fn rate_schedule(schedule: &RateSchedule, at: &At<'_>, errors: &mut Vec<PlanError>) {
    match schedule {
        RateSchedule::Fixed {
            rate,
            duration: length,
        } => {
            fixed_rate(*rate, &at.key("rate"), errors);
            duration(*length, true, &at.key("duration"), errors);
        }
        RateSchedule::Ramping { start, stages } => {
            ramp(start.count > 0, stages, |to| to.count > 0, at, errors);
        }
    }
}

/// Checks the stages of a ramp. `start_above_zero` says whether the start is
/// above zero.
fn ramp<T>(
    start_above_zero: bool,
    stages: &[Stage<T>],
    above_zero: impl Fn(&T) -> bool,
    at: &At<'_>,
    errors: &mut Vec<PlanError>,
) {
    let stages_at = at.key("stages");
    if stages.is_empty() {
        push(errors, &stages_at, PlanErrorKind::NoStages);
        return;
    }
    let mut total = Duration::ZERO;
    for (index, stage) in stages.iter().enumerate() {
        duration(
            stage.over,
            false,
            &stages_at.index(index).key("over"),
            errors,
        );
        total = total.saturating_add(stage.over);
    }
    if !start_above_zero && !stages.iter().any(|stage| above_zero(&stage.to)) {
        push(errors, &stages_at, PlanErrorKind::IdleRamp);
    }
    if total.is_zero() {
        push(errors, &stages_at, PlanErrorKind::ZeroLengthRamp);
    }
}

/// Methods that may be retried without `allow_non_idempotent`.
pub(super) const IDEMPOTENT: [&str; 6] = ["GET", "HEAD", "PUT", "DELETE", "OPTIONS", "TRACE"];

/// Checks a request name. `at` is the request; problems are reported at `name`.
pub(super) fn request_name(name: Option<&str>, at: &At<'_>, errors: &mut Vec<PlanError>) {
    if name.is_some_and(|name| name.contains("${")) {
        push(errors, &at.key("name"), PlanErrorKind::TemplateInName);
    }
}

/// Checks a request target. `at` is the request; problems are reported at `path`.
pub(super) fn target(
    target: &RequestTarget,
    has_base_url: bool,
    at: &At<'_>,
    errors: &mut Vec<PlanError>,
) {
    if matches!(target, RequestTarget::Path(_)) && !has_base_url {
        push(errors, &at.key("path"), PlanErrorKind::PathNeedsBaseUrl);
    }
}

/// Checks a retry policy. `at` is the `retry` place.
pub(super) fn retry(
    retry: &RetryPolicy,
    method: &HttpMethod,
    at: &At<'_>,
    errors: &mut Vec<PlanError>,
) {
    if retry.attempts < 2 {
        push(errors, &at.key("attempts"), PlanErrorKind::TooFewAttempts);
    }
    duration(retry.backoff, false, &at.key("backoff"), errors);
    if !retry.allow_non_idempotent && !IDEMPOTENT.contains(&method.as_str()) {
        push(errors, at, PlanErrorKind::RetryNeedsOptIn);
    }
}

/// Checks a status match. `at` is the `expect.status` place.
pub(super) fn status(status: &StatusMatch, at: &At<'_>, errors: &mut Vec<PlanError>) {
    if let StatusMatch::OneOf(codes) = status {
        if codes.is_empty() {
            push(errors, at, PlanErrorKind::EmptyStatus);
        } else if codes.iter().any(|code| !(100..=599).contains(code)) {
            push(errors, at, PlanErrorKind::StatusOutOfRange);
        }
    }
}

/// Checks the list of an `all` or `any`. `at` is that key.
pub(super) fn condition_list(conditions: &[Condition], at: &At<'_>, errors: &mut Vec<PlanError>) {
    if conditions.is_empty() {
        push(errors, at, PlanErrorKind::NoConditions);
    }
}

/// Checks that a threshold's scenario exists. `at` is the threshold.
pub(super) fn threshold_scenario(
    scenario: Option<&str>,
    names: &HashSet<&str>,
    at: &At<'_>,
    errors: &mut Vec<PlanError>,
) {
    if scenario.is_some_and(|name| !names.contains(name)) {
        push(errors, &at.key("scenario"), PlanErrorKind::UnknownScenario);
    }
}

/// Checks that a number in a `json` body or a comparison is finite. `at` is
/// the number's own place.
pub(super) fn number(number: JsonNumber, at: &At<'_>, errors: &mut Vec<PlanError>) {
    if let JsonNumber::Float(value) = number
        && !value.is_finite()
    {
        push(errors, at, PlanErrorKind::NonFiniteNumber);
    }
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};

    use super::super::super::load::QueuePolicy;
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn run(check: impl FnOnce(&At<'_>, &mut Vec<PlanError>)) -> Vec<(String, PlanErrorKind)> {
        let mut errors = Vec::new();
        check(&At::Root.key("x"), &mut errors);
        errors
            .into_iter()
            .map(|e| (e.path.to_string(), e.kind))
            .collect()
    }

    fn rate(text: &str) -> Rate {
        text.parse().unwrap()
    }

    fn vus() -> NonZeroU32 {
        NonZeroU32::new(1).unwrap()
    }

    fn rate_loads(schedule: RateSchedule) -> [LoadModel; 2] {
        [
            LoadModel::IterationRate {
                schedule: schedule.clone(),
                max_vus: vus(),
                queue: QueuePolicy::default(),
            },
            LoadModel::RequestRate {
                schedule,
                vus: vus(),
            },
        ]
    }

    fn at_x(kind: PlanErrorKind, sub: &str) -> Vec<(String, PlanErrorKind)> {
        vec![(format!("x.{sub}"), kind)]
    }

    #[test]
    fn durations_at_the_ceiling_pass_and_one_millisecond_more_fails() {
        assert!(run(|at, e| duration(MAX_DURATION, true, at, e)).is_empty());
        assert_eq!(
            run(|at, e| duration(MAX_DURATION + MS, true, at, e)),
            [("x".to_string(), PlanErrorKind::DurationTooLong)]
        );
        assert_eq!(
            run(|at, e| duration(Duration::ZERO, true, at, e)),
            [("x".to_string(), PlanErrorKind::ZeroDuration)]
        );
        assert!(run(|at, e| duration(Duration::ZERO, false, at, e)).is_empty());
        assert_eq!(
            run(|at, e| duration(Duration::from_millis(u64::MAX), false, at, e)),
            [("x".to_string(), PlanErrorKind::DurationTooLong)]
        );
    }

    #[test]
    fn inherited_values_are_checked_only_where_they_differ() {
        assert!(run(|at, e| inherited(Duration::ZERO, Duration::ZERO, at, e)).is_empty());
        assert_eq!(
            run(|at, e| inherited(Duration::ZERO, Duration::from_secs(1), at, e)),
            [("x".to_string(), PlanErrorKind::ZeroDuration)]
        );
    }

    #[test]
    fn fixed_rates_must_be_above_zero() {
        for zero in ["0/s", "0/m"] {
            let schedule = RateSchedule::Fixed {
                rate: rate(zero),
                duration: Duration::from_secs(1),
            };
            for model in rate_loads(schedule) {
                assert_eq!(
                    run(|at, e| load(&model, at, e)),
                    at_x(PlanErrorKind::ZeroRate, "rate")
                );
            }
        }
        let schedule = RateSchedule::Fixed {
            rate: rate("1/h"),
            duration: Duration::from_secs(1),
        };
        for model in rate_loads(schedule) {
            assert!(run(|at, e| load(&model, at, e)).is_empty());
        }
        assert_eq!(
            run(|at, e| rate_budget(Some(rate("0/s")), at, e)),
            [("x".to_string(), PlanErrorKind::ZeroRate)]
        );
        assert!(run(|at, e| rate_budget(None, at, e)).is_empty());
    }

    #[test]
    fn fixed_load_durations_must_be_above_zero() {
        let closed = LoadModel::ClosedVus(ClosedVusLoad::Fixed {
            vus: vus(),
            stop: ClosedStop::Duration(Duration::ZERO),
        });
        assert_eq!(
            run(|at, e| load(&closed, at, e)),
            at_x(PlanErrorKind::ZeroDuration, "duration")
        );
        let schedule = RateSchedule::Fixed {
            rate: rate("1/s"),
            duration: Duration::ZERO,
        };
        for model in rate_loads(schedule) {
            assert_eq!(
                run(|at, e| load(&model, at, e)),
                at_x(PlanErrorKind::ZeroDuration, "duration")
            );
        }
        let iterations = LoadModel::ClosedVus(ClosedVusLoad::Fixed {
            vus: vus(),
            stop: ClosedStop::IterationsPerVu(NonZeroU64::new(1).unwrap()),
        });
        assert!(run(|at, e| load(&iterations, at, e)).is_empty());
    }

    fn stage<T>(to: T, over: Duration) -> Stage<T> {
        Stage { to, over }
    }

    fn closed_ramp(start: u32, stages: Vec<Stage<u32>>) -> LoadModel {
        LoadModel::ClosedVus(ClosedVusLoad::Ramping { start, stages })
    }

    #[test]
    fn ramps_must_rise_above_zero() {
        let s = Duration::from_secs(1);
        let idle = [
            closed_ramp(0, vec![stage(0, s), stage(0, s)]),
            rate_loads(RateSchedule::Ramping {
                start: rate("0/s"),
                stages: vec![stage(rate("0/m"), s)],
            })[0]
                .clone(),
        ];
        for model in idle {
            assert_eq!(
                run(|at, e| load(&model, at, e)),
                at_x(PlanErrorKind::IdleRamp, "stages")
            );
        }
        for model in [
            closed_ramp(5, vec![stage(0, s)]),
            closed_ramp(0, vec![stage(0, s), stage(3, s)]),
        ] {
            assert!(run(|at, e| load(&model, at, e)).is_empty());
        }
    }

    #[test]
    fn ramps_must_last_longer_than_zero() {
        let zero = closed_ramp(1, vec![stage(1, Duration::ZERO)]);
        assert_eq!(
            run(|at, e| load(&zero, at, e)),
            at_x(PlanErrorKind::ZeroLengthRamp, "stages")
        );
        let short = closed_ramp(1, vec![stage(1, Duration::ZERO), stage(1, MS)]);
        assert!(run(|at, e| load(&short, at, e)).is_empty());
        let huge = Duration::from_millis(u64::MAX);
        let long = closed_ramp(1, vec![stage(1, huge), stage(1, huge)]);
        assert_eq!(
            run(|at, e| load(&long, at, e)),
            [
                (
                    "x.stages[0].over".to_string(),
                    PlanErrorKind::DurationTooLong
                ),
                (
                    "x.stages[1].over".to_string(),
                    PlanErrorKind::DurationTooLong
                ),
            ]
        );
    }

    #[test]
    fn empty_ramps_report_only_no_stages() {
        let mut models = vec![closed_ramp(0, vec![])];
        models.extend(rate_loads(RateSchedule::Ramping {
            start: rate("0/s"),
            stages: vec![],
        }));
        for model in models {
            assert_eq!(
                run(|at, e| load(&model, at, e)),
                at_x(PlanErrorKind::NoStages, "stages")
            );
        }
    }

    #[test]
    fn queue_max_wait_may_be_zero_but_not_too_long() {
        let schedule = RateSchedule::Fixed {
            rate: rate("1/s"),
            duration: Duration::from_secs(1),
        };
        let with_wait = |max_wait| LoadModel::IterationRate {
            schedule: schedule.clone(),
            max_vus: vus(),
            queue: QueuePolicy {
                max_wait,
                ..QueuePolicy::default()
            },
        };
        assert!(run(|at, e| load(&with_wait(Duration::ZERO), at, e)).is_empty());
        assert_eq!(
            run(|at, e| load(&with_wait(MAX_DURATION + MS), at, e)),
            at_x(PlanErrorKind::DurationTooLong, "queue.max_wait")
        );
    }
}
