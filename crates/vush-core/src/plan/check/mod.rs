//! The structural checks that run when a draft becomes a plan.
//!
//! The walk visits a draft in plan order and passes down a borrowed [`At`]
//! path, so nothing is allocated until a problem is recorded.

use std::collections::HashSet;
use std::time::Duration;

use super::error::{PathSegment, PlanError, PlanErrorKind, PlanPath};
use super::step::Step;
use super::threshold::{BoundOp, Threshold, ThresholdCheck};
use super::workload::{PlanDraft, Scenario};

mod values;

/// A place in a draft, as a chain of borrowed links back to the root.
#[derive(Clone, Copy)]
pub(super) enum At<'a> {
    Root,
    Key(&'a At<'a>, &'a str),
    Index(&'a At<'a>, usize),
}

impl<'a> At<'a> {
    /// The place under this one at a map key.
    pub(super) fn key(&'a self, key: &'a str) -> At<'a> {
        At::Key(self, key)
    }

    /// The place under this one at a list position.
    pub(super) fn index(&'a self, index: usize) -> At<'a> {
        At::Index(self, index)
    }

    /// Builds the owned path. Only called when a problem is recorded.
    pub(super) fn path(&self) -> PlanPath {
        let mut segments = Vec::new();
        let mut at = self;
        loop {
            match at {
                At::Root => break,
                At::Key(parent, key) => {
                    segments.push(PathSegment::Key((*key).to_owned()));
                    at = parent;
                }
                At::Index(parent, index) => {
                    segments.push(PathSegment::Index(*index));
                    at = parent;
                }
            }
        }
        segments.reverse();
        PlanPath::new(segments)
    }
}

/// Records a problem at a place.
pub(super) fn push(errors: &mut Vec<PlanError>, at: &At<'_>, kind: PlanErrorKind) {
    errors.push(PlanError {
        path: at.path(),
        kind,
    });
}

/// What the walk carries down: the `http.timeout` that requests inherit.
#[derive(Clone, Copy)]
struct Scope {
    request_timeout: Duration,
}

/// Finds every problem in a draft, in plan order: file-level `http`,
/// `limits`, `setup` and `teardown`, then `thresholds`, then `scenarios`.
pub(super) fn check(draft: &PlanDraft) -> Vec<PlanError> {
    let mut errors = Vec::new();
    let root = At::Root;
    let scope = Scope {
        request_timeout: draft.http.timeout,
    };
    values::duration(
        draft.http.timeout,
        true,
        &root.key("http").key("timeout"),
        &mut errors,
    );
    let limits = root.key("limits");
    values::duration(
        draft.limits.hook_timeout,
        true,
        &limits.key("hook_timeout"),
        &mut errors,
    );
    values::rate_budget(
        draft.max_request_rate,
        &limits.key("max_request_rate"),
        &mut errors,
    );
    check_steps(&draft.setup, &root.key("setup"), scope, &mut errors);
    check_steps(&draft.teardown, &root.key("teardown"), scope, &mut errors);
    let thresholds = root.key("thresholds");
    for (index, threshold) in draft.thresholds.iter().enumerate() {
        check_threshold(threshold, &thresholds.index(index), &mut errors);
    }
    let scenarios = root.key("scenarios");
    if draft.scenarios.is_empty() {
        push(&mut errors, &scenarios, PlanErrorKind::NoScenarios);
    }
    let mut seen_names = HashSet::new();
    for (index, scenario) in draft.scenarios.iter().enumerate() {
        check_scenario(
            draft,
            scenario,
            &scenarios.index(index),
            &mut seen_names,
            &mut errors,
        );
    }
    errors
}

/// Checks one threshold, whose own place is `at`.
fn check_threshold(threshold: &Threshold, at: &At<'_>, errors: &mut Vec<PlanError>) {
    if let ThresholdCheck::HttpRequestDuration { bound, .. } = &threshold.check {
        let key = match bound.op {
            BoundOp::Lt => "lt",
            BoundOp::Lte => "lte",
            BoundOp::Gt => "gt",
            BoundOp::Gte => "gte",
        };
        values::duration(bound.value, false, &at.key(key), errors);
    }
}

/// Checks one scenario, whose own place is `at`.
fn check_scenario<'a>(
    draft: &PlanDraft,
    scenario: &'a Scenario,
    at: &At<'_>,
    seen_names: &mut HashSet<&'a str>,
    errors: &mut Vec<PlanError>,
) {
    let scope = Scope {
        request_timeout: scenario.http.timeout,
    };
    if !seen_names.insert(scenario.name.as_str()) {
        push(
            errors,
            &at.key("name"),
            PlanErrorKind::DuplicateScenarioName,
        );
    }
    values::load(&scenario.load, &at.key("load"), errors);
    values::inherited(
        scenario.http.timeout,
        draft.http.timeout,
        &at.key("http").key("timeout"),
        errors,
    );
    let limits = at.key("limits");
    values::inherited(
        scenario.limits.hook_timeout,
        draft.limits.hook_timeout,
        &limits.key("hook_timeout"),
        errors,
    );
    values::rate_budget(
        scenario.max_request_rate,
        &limits.key("max_request_rate"),
        errors,
    );
    check_steps(&scenario.setup, &at.key("setup"), scope, errors);
    check_steps(&scenario.teardown, &at.key("teardown"), scope, errors);
    check_steps(&scenario.vu_setup, &at.key("vu_setup"), scope, errors);
    check_steps(&scenario.vu_teardown, &at.key("vu_teardown"), scope, errors);
    let steps = at.key("steps");
    if scenario.steps.is_empty() {
        push(errors, &steps, PlanErrorKind::NoSteps);
    }
    check_steps(&scenario.steps, &steps, scope, errors);
}

/// Checks every step in a list, and the lists nested inside them.
///
/// `at` is the list's own place, such as `scenarios[0].steps`. This does not
/// check that the list itself is non-empty: the caller knows whether it may be
/// empty, and reports [`PlanErrorKind::NoSteps`] when it may not.
fn check_steps(steps: &[Step], at: &At<'_>, scope: Scope, errors: &mut Vec<PlanError>) {
    for (index, step) in steps.iter().enumerate() {
        let step_at = at.index(index);
        match step {
            Step::Request(request) => {
                let request_at = step_at.key("request");
                values::inherited(
                    request.timeout,
                    scope.request_timeout,
                    &request_at.key("timeout"),
                    errors,
                );
                if let Some(retry) = &request.retry {
                    values::duration(
                        retry.backoff,
                        false,
                        &request_at.key("retry").key("backoff"),
                        errors,
                    );
                }
            }
            Step::Wait(value) => values::duration(*value, false, &step_at.key("wait"), errors),
            Step::If(step) => {
                let if_at = step_at.key("if");
                let then = if_at.key("then");
                if step.then.is_empty() {
                    push(errors, &then, PlanErrorKind::NoSteps);
                }
                check_steps(&step.then, &then, scope, errors);
                check_steps(&step.otherwise, &if_at.key("else"), scope, errors);
            }
            Step::Repeat(step) => {
                let repeat_at = step_at.key("repeat");
                values::duration(step.deadline, true, &repeat_at.key("deadline"), errors);
                let inner = repeat_at.key("steps");
                if step.steps.is_empty() {
                    push(errors, &inner, PlanErrorKind::NoSteps);
                }
                check_steps(&step.steps, &inner, scope, errors);
            }
        }
    }
}

#[cfg(test)]
pub(super) mod testing {
    use std::num::NonZeroU32;
    use std::time::Duration;

    use super::super::load::{ClosedStop, ClosedVusLoad, LoadModel};
    use super::super::settings::{HttpSettings, ResourceLimits};
    use super::super::step::{Expect, RequestStep, RequestTarget, Step};
    use super::super::template::Template;
    use super::super::workload::{PlanDraft, Scenario, StatePolicy};
    use super::{PlanErrorKind, check};

    pub(in super::super) fn wait() -> Step {
        Step::Wait(Duration::from_secs(1))
    }

    pub(in super::super) fn tpl(text: &str) -> Template {
        text.parse().unwrap()
    }

    pub(in super::super) fn request(method: &str, target: RequestTarget) -> RequestStep {
        RequestStep {
            name: None,
            method: method.parse().unwrap(),
            target,
            query: vec![],
            headers: vec![],
            body: None,
            timeout: HttpSettings::default().timeout,
            required: true,
            retry: None,
            expect: Expect::default(),
            extract: vec![],
        }
    }

    pub(in super::super) fn scenario(name: &str) -> Scenario {
        Scenario {
            name: name.to_string(),
            load: LoadModel::ClosedVus(ClosedVusLoad::Fixed {
                vus: NonZeroU32::new(1).unwrap(),
                stop: ClosedStop::Duration(Duration::from_secs(1)),
            }),
            http: HttpSettings::default(),
            limits: ResourceLimits::default(),
            max_request_rate: None,
            state: StatePolicy::Persist,
            setup: vec![],
            teardown: vec![],
            vu_setup: vec![],
            vu_teardown: vec![],
            steps: vec![wait()],
        }
    }

    pub(in super::super) fn draft(scenarios: Vec<Scenario>) -> PlanDraft {
        PlanDraft {
            name: "test".to_string(),
            base_url: None,
            http: HttpSettings::default(),
            limits: ResourceLimits::default(),
            max_request_rate: None,
            setup: vec![],
            teardown: vec![],
            thresholds: vec![],
            scenarios,
        }
    }

    /// Runs the checks and returns each error's path as text with its kind.
    pub(in super::super) fn errors(draft: &PlanDraft) -> Vec<(String, PlanErrorKind)> {
        check(draft)
            .into_iter()
            .map(|e| (e.path.to_string(), e.kind))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::time::Duration;

    use super::super::load::{ClosedVusLoad, LoadModel, QueuePolicy, RateSchedule};
    use super::super::settings::{HttpSettings, ResourceLimits};
    use super::super::step::{Comparison, Condition, IfStep, RepeatStep, Step};
    use super::super::step::{RequestStep, RequestTarget, RetryPolicy};
    use super::super::template::VarRef;
    use super::super::threshold::{Bound, PercentileStat};
    use super::super::value::{MAX_DURATION, Rate};
    use super::testing::{draft, errors, request, scenario, tpl, wait};
    use super::*;

    #[test]
    fn at_builds_its_path_on_request() {
        let root = At::Root;
        let a = root.key("a");
        let a2 = a.index(2);
        let b = a2.key("b");
        assert_eq!(b.path().to_string(), "a[2].b");
    }

    fn repeat(steps: Vec<Step>) -> Step {
        Step::Repeat(RepeatStep {
            count: NonZeroU32::new(1).unwrap(),
            deadline: Duration::from_secs(60),
            steps,
        })
    }

    fn if_then(then: Vec<Step>, otherwise: Vec<Step>) -> Step {
        Step::If(IfStep {
            when: Condition::Compare {
                var: VarRef::VuId,
                op: Comparison::Exists(true),
            },
            then,
            otherwise,
        })
    }

    #[test]
    fn no_scenarios_is_reported() {
        assert_eq!(
            errors(&draft(vec![])),
            [("scenarios".to_string(), PlanErrorKind::NoScenarios)]
        );
    }

    #[test]
    fn duplicate_scenario_name_is_reported_at_later_occurrence() {
        let d = draft(vec![scenario("a"), scenario("b"), scenario("a")]);
        assert_eq!(
            errors(&d),
            [(
                "scenarios[2].name".to_string(),
                PlanErrorKind::DuplicateScenarioName
            )]
        );
    }

    #[test]
    fn empty_scenario_steps_are_reported() {
        let mut s = scenario("a");
        s.steps.clear();
        assert_eq!(
            errors(&draft(vec![s])),
            [("scenarios[0].steps".to_string(), PlanErrorKind::NoSteps)]
        );
    }

    #[test]
    fn empty_nested_step_lists_are_found_with_full_paths() {
        let mut s = scenario("a");
        s.steps = vec![
            repeat(vec![]),
            if_then(vec![], vec![]),
            if_then(vec![wait()], vec![repeat(vec![])]),
        ];
        assert_eq!(
            errors(&draft(vec![s])),
            [
                (
                    "scenarios[0].steps[0].repeat.steps".to_string(),
                    PlanErrorKind::NoSteps
                ),
                (
                    "scenarios[0].steps[1].if.then".to_string(),
                    PlanErrorKind::NoSteps
                ),
                (
                    "scenarios[0].steps[2].if.else[0].repeat.steps".to_string(),
                    PlanErrorKind::NoSteps
                ),
            ]
        );
    }

    #[test]
    fn nested_empty_lists_inside_hooks_are_found() {
        let mut s = scenario("a");
        s.vu_teardown = vec![repeat(vec![])];
        let mut d = draft(vec![s]);
        d.setup = vec![if_then(vec![], vec![])];
        assert_eq!(
            errors(&d),
            [
                ("setup[0].if.then".to_string(), PlanErrorKind::NoSteps),
                (
                    "scenarios[0].vu_teardown[0].repeat.steps".to_string(),
                    PlanErrorKind::NoSteps
                ),
            ]
        );
    }

    #[test]
    fn empty_stages_are_reported_for_every_ramping_model() {
        let ramping_rate = RateSchedule::Ramping {
            start: "0/s".parse().unwrap(),
            stages: vec![],
        };
        let vus = NonZeroU32::new(1).unwrap();
        for load in [
            LoadModel::ClosedVus(ClosedVusLoad::Ramping {
                start: 0,
                stages: vec![],
            }),
            LoadModel::IterationRate {
                schedule: ramping_rate.clone(),
                max_vus: vus,
                queue: QueuePolicy::default(),
            },
            LoadModel::RequestRate {
                schedule: ramping_rate.clone(),
                vus,
            },
        ] {
            let mut s = scenario("a");
            s.load = load;
            assert_eq!(
                errors(&draft(vec![s])),
                [(
                    "scenarios[0].load.stages".to_string(),
                    PlanErrorKind::NoStages
                )]
            );
        }
    }

    #[test]
    fn empty_hooks_and_empty_else_are_allowed() {
        let mut s = scenario("a");
        s.setup = vec![];
        s.steps = vec![if_then(vec![wait()], vec![])];
        assert!(check(&draft(vec![s])).is_empty());
    }

    #[test]
    fn several_problems_are_reported_together_in_document_order() {
        let mut first = scenario("a");
        first.load = LoadModel::ClosedVus(ClosedVusLoad::Ramping {
            start: 0,
            stages: vec![],
        });
        first.steps.clear();
        let mut d = draft(vec![first, scenario("a")]);
        d.setup = vec![repeat(vec![])];
        assert_eq!(
            errors(&d),
            [
                ("setup[0].repeat.steps".to_string(), PlanErrorKind::NoSteps),
                (
                    "scenarios[0].load.stages".to_string(),
                    PlanErrorKind::NoStages
                ),
                ("scenarios[0].steps".to_string(), PlanErrorKind::NoSteps),
                (
                    "scenarios[1].name".to_string(),
                    PlanErrorKind::DuplicateScenarioName
                ),
            ]
        );
    }

    #[test]
    fn every_list_is_walked_in_document_order() {
        let mut first = scenario("a");
        first.setup = vec![repeat(vec![])];
        first.teardown = vec![repeat(vec![])];
        first.vu_setup = vec![repeat(vec![])];
        first.vu_teardown = vec![repeat(vec![])];
        first.steps = vec![if_then(vec![repeat(vec![])], vec![repeat(vec![])])];
        let mut second = scenario("a");
        second.load = LoadModel::ClosedVus(ClosedVusLoad::Ramping {
            start: 0,
            stages: vec![],
        });
        second.setup = vec![repeat(vec![])];
        second.steps.clear();
        let mut d = draft(vec![first, second, scenario("a")]);
        d.setup = vec![repeat(vec![])];
        d.teardown = vec![repeat(vec![])];
        let paths: Vec<String> = errors(&d).into_iter().map(|(path, _)| path).collect();
        assert_eq!(
            paths,
            [
                "setup[0].repeat.steps",
                "teardown[0].repeat.steps",
                "scenarios[0].setup[0].repeat.steps",
                "scenarios[0].teardown[0].repeat.steps",
                "scenarios[0].vu_setup[0].repeat.steps",
                "scenarios[0].vu_teardown[0].repeat.steps",
                "scenarios[0].steps[0].if.then[0].repeat.steps",
                "scenarios[0].steps[0].if.else[0].repeat.steps",
                "scenarios[1].name",
                "scenarios[1].load.stages",
                "scenarios[1].setup[0].repeat.steps",
                "scenarios[1].steps",
                "scenarios[2].name",
            ]
        );
    }

    #[test]
    fn empty_list_is_found_64_levels_deep() {
        let mut inner = vec![];
        for _ in 0..64 {
            inner = vec![repeat(inner)];
        }
        let mut s = scenario("a");
        s.steps = inner;
        let want = format!("scenarios[0].steps{}", "[0].repeat.steps".repeat(64));
        assert_eq!(errors(&draft(vec![s])), [(want, PlanErrorKind::NoSteps)]);
    }

    const MS: Duration = Duration::from_millis(1);

    fn req(timeout: Duration) -> RequestStep {
        let mut r = request("GET", RequestTarget::Url(tpl("http://localhost/")));
        r.timeout = timeout;
        r
    }

    fn req_step(timeout: Duration) -> Step {
        Step::Request(Box::new(req(timeout)))
    }

    fn rate(text: &str) -> Rate {
        text.parse().unwrap()
    }

    #[test]
    fn every_duration_is_checked_at_its_path() {
        let long = MAX_DURATION + MS;
        let mut teardown = req(MAX_DURATION + MS * 2);
        teardown.retry = Some(RetryPolicy {
            attempts: 2,
            backoff: long,
            allow_non_idempotent: false,
        });
        let mut s = scenario("a");
        s.load = LoadModel::IterationRate {
            schedule: RateSchedule::Ramping {
                start: rate("0/s"),
                stages: vec![super::super::load::Stage {
                    to: rate("1/s"),
                    over: long,
                }],
            },
            max_vus: NonZeroU32::new(1).unwrap(),
            queue: QueuePolicy {
                max_wait: long,
                ..QueuePolicy::default()
            },
        };
        s.http = HttpSettings {
            timeout: MAX_DURATION + MS * 3,
            ..HttpSettings::default()
        };
        s.limits = ResourceLimits {
            hook_timeout: MAX_DURATION + MS * 4,
            ..ResourceLimits::default()
        };
        s.steps = vec![Step::Repeat(RepeatStep {
            count: NonZeroU32::new(1).unwrap(),
            deadline: long,
            steps: vec![Step::Wait(long)],
        })];
        let mut d = draft(vec![s]);
        d.http.timeout = long;
        d.limits.hook_timeout = long;
        d.setup = vec![Step::Wait(long)];
        d.teardown = vec![Step::Request(Box::new(teardown))];
        d.thresholds = vec![Threshold {
            scenario: None,
            check: ThresholdCheck::HttpRequestDuration {
                stat: PercentileStat::P95,
                bound: Bound {
                    op: BoundOp::Lt,
                    value: long,
                },
            },
        }];
        let got = errors(&d);
        let paths: Vec<&str> = got.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            [
                "http.timeout",
                "limits.hook_timeout",
                "setup[0].wait",
                "teardown[0].request.timeout",
                "teardown[0].request.retry.backoff",
                "thresholds[0].lt",
                "scenarios[0].load.stages[0].over",
                "scenarios[0].load.queue.max_wait",
                "scenarios[0].http.timeout",
                "scenarios[0].limits.hook_timeout",
                "scenarios[0].steps[0].repeat.deadline",
                "scenarios[0].steps[0].repeat.steps[0].wait",
            ]
        );
        assert!(
            got.iter()
                .all(|(_, k)| *k == PlanErrorKind::DurationTooLong)
        );
    }

    #[test]
    fn inherited_timeouts_are_reported_once() {
        let nested = || {
            let mut s = scenario("x");
            s.steps = vec![if_then(vec![req_step(Duration::ZERO)], vec![])];
            s
        };
        let mut a = nested();
        a.name = "a".into();
        a.http.timeout = Duration::ZERO;
        a.limits.hook_timeout = Duration::ZERO;
        let mut b = nested();
        b.name = "b".into();
        b.http.timeout = Duration::ZERO;
        b.limits.hook_timeout = Duration::ZERO;
        let mut d = draft(vec![a, b]);
        d.http.timeout = Duration::ZERO;
        d.limits.hook_timeout = Duration::ZERO;
        assert_eq!(
            errors(&d),
            [
                ("http.timeout".to_string(), PlanErrorKind::ZeroDuration),
                (
                    "limits.hook_timeout".to_string(),
                    PlanErrorKind::ZeroDuration
                ),
            ]
        );

        let mut s = scenario("a");
        s.http.timeout = MAX_DURATION + MS;
        s.limits.hook_timeout = Duration::ZERO;
        s.steps = vec![req_step(Duration::ZERO)];
        let d = draft(vec![scenario("z"), s]);
        assert_eq!(
            errors(&d),
            [
                (
                    "scenarios[1].http.timeout".to_string(),
                    PlanErrorKind::DurationTooLong
                ),
                (
                    "scenarios[1].limits.hook_timeout".to_string(),
                    PlanErrorKind::ZeroDuration
                ),
                (
                    "scenarios[1].steps[0].request.timeout".to_string(),
                    PlanErrorKind::ZeroDuration
                ),
            ]
        );
    }

    #[test]
    fn max_request_rate_must_be_above_zero() {
        let mut s = scenario("a");
        s.max_request_rate = Some(rate("0/s"));
        let mut d = draft(vec![s]);
        d.max_request_rate = Some(rate("0/s"));
        assert_eq!(
            errors(&d),
            [
                (
                    "limits.max_request_rate".to_string(),
                    PlanErrorKind::ZeroRate
                ),
                (
                    "scenarios[0].limits.max_request_rate".to_string(),
                    PlanErrorKind::ZeroRate
                ),
            ]
        );
    }

    #[test]
    fn zero_waits_and_backoffs_are_allowed() {
        let mut r = req(Duration::from_secs(1));
        r.retry = Some(RetryPolicy {
            attempts: 2,
            backoff: Duration::ZERO,
            allow_non_idempotent: false,
        });
        let mut s = scenario("a");
        s.steps = vec![Step::Wait(Duration::ZERO), Step::Request(Box::new(r))];
        assert!(check(&draft(vec![s])).is_empty());
    }
}
