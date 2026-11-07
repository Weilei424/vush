//! The structural checks that run when a draft becomes a plan.
//!
//! The walk visits a draft in plan order and passes down a borrowed [`At`]
//! path, so nothing is allocated until a problem is recorded.

use std::collections::HashSet;

use super::error::{PathSegment, PlanError, PlanErrorKind, PlanPath};
use super::load::{ClosedVusLoad, LoadModel, RateSchedule};
use super::step::Step;
use super::workload::{PlanDraft, Scenario};

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

/// Finds every problem in a draft, in plan order: file-level `setup` and
/// `teardown`, then `scenarios`.
pub(super) fn check(draft: &PlanDraft) -> Vec<PlanError> {
    let mut errors = Vec::new();
    let root = At::Root;
    check_steps(&draft.setup, &root.key("setup"), &mut errors);
    check_steps(&draft.teardown, &root.key("teardown"), &mut errors);
    let scenarios = root.key("scenarios");
    if draft.scenarios.is_empty() {
        push(&mut errors, &scenarios, PlanErrorKind::NoScenarios);
    }
    let mut seen_names = HashSet::new();
    for (index, scenario) in draft.scenarios.iter().enumerate() {
        check_scenario(
            scenario,
            &scenarios.index(index),
            &mut seen_names,
            &mut errors,
        );
    }
    errors
}

/// Checks one scenario, whose own place is `at`.
fn check_scenario<'a>(
    scenario: &'a Scenario,
    at: &At<'_>,
    seen_names: &mut HashSet<&'a str>,
    errors: &mut Vec<PlanError>,
) {
    if !seen_names.insert(scenario.name.as_str()) {
        push(
            errors,
            &at.key("name"),
            PlanErrorKind::DuplicateScenarioName,
        );
    }
    check_load(&scenario.load, &at.key("load"), errors);
    check_steps(&scenario.setup, &at.key("setup"), errors);
    check_steps(&scenario.teardown, &at.key("teardown"), errors);
    check_steps(&scenario.vu_setup, &at.key("vu_setup"), errors);
    check_steps(&scenario.vu_teardown, &at.key("vu_teardown"), errors);
    let steps = at.key("steps");
    if scenario.steps.is_empty() {
        push(errors, &steps, PlanErrorKind::NoSteps);
    }
    check_steps(&scenario.steps, &steps, errors);
}

/// Checks a load model, whose own place is `at`.
fn check_load(load: &LoadModel, at: &At<'_>, errors: &mut Vec<PlanError>) {
    let no_stages = match load {
        LoadModel::ClosedVus(ClosedVusLoad::Fixed { .. }) => false,
        LoadModel::ClosedVus(ClosedVusLoad::Ramping { stages, .. }) => stages.is_empty(),
        LoadModel::IterationRate { schedule, .. } | LoadModel::RequestRate { schedule, .. } => {
            match schedule {
                RateSchedule::Fixed { .. } => false,
                RateSchedule::Ramping { stages, .. } => stages.is_empty(),
            }
        }
    };
    if no_stages {
        push(errors, &at.key("stages"), PlanErrorKind::NoStages);
    }
}

/// Checks every step in a list, and the lists nested inside them.
///
/// `at` is the list's own place, such as `scenarios[0].steps`. This does not
/// check that the list itself is non-empty: the caller knows whether it may be
/// empty, and reports [`PlanErrorKind::NoSteps`] when it may not.
fn check_steps(steps: &[Step], at: &At<'_>, errors: &mut Vec<PlanError>) {
    for (index, step) in steps.iter().enumerate() {
        let step_at = at.index(index);
        match step {
            Step::Request(_) | Step::Wait(_) => {}
            Step::If(step) => {
                let if_at = step_at.key("if");
                let then = if_at.key("then");
                if step.then.is_empty() {
                    push(errors, &then, PlanErrorKind::NoSteps);
                }
                check_steps(&step.then, &then, errors);
                check_steps(&step.otherwise, &if_at.key("else"), errors);
            }
            Step::Repeat(step) => {
                let repeat_at = step_at.key("repeat");
                let inner = repeat_at.key("steps");
                if step.steps.is_empty() {
                    push(errors, &inner, PlanErrorKind::NoSteps);
                }
                check_steps(&step.steps, &inner, errors);
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
    use super::super::step::Step;
    use super::super::workload::{PlanDraft, Scenario, StatePolicy};
    use super::{PlanErrorKind, check};

    pub(in super::super) fn wait() -> Step {
        Step::Wait(Duration::from_secs(1))
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
    use super::super::step::{Comparison, Condition, IfStep, RepeatStep, Step};
    use super::super::template::VarRef;
    use super::testing::{draft, errors, scenario, wait};
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
}
