//! The workload plan and the one checked constructor that builds it.

use std::collections::HashSet;
use std::fmt;

use super::load::{ClosedVusLoad, LoadModel, RateSchedule};
use super::settings::{HttpSettings, ResourceLimits};
use super::step::Step;
use super::template::Template;
use super::threshold::Threshold;
use super::value::Rate;

/// What an adapter fills in. It is only a draft: nothing runs it until
/// [`WorkloadPlan::new`] has checked it.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanDraft {
    pub name: String,
    pub base_url: Option<Template>,
    pub http: HttpSettings,
    pub limits: ResourceLimits,
    /// A request-rate budget across all scenarios.
    pub max_request_rate: Option<Rate>,
    /// Runs once per run, before any scenario.
    pub setup: Vec<Step>,
    /// Runs once per run, after every scenario.
    pub teardown: Vec<Step>,
    pub thresholds: Vec<Threshold>,
    pub scenarios: Vec<Scenario>,
}

/// One scenario: a load model and the steps it drives. Defaults and file-level
/// settings are already merged in.
#[derive(Debug, Clone, PartialEq)]
pub struct Scenario {
    /// Unique within a plan.
    pub name: String,
    pub load: LoadModel,
    pub http: HttpSettings,
    pub limits: ResourceLimits,
    /// This scenario's own request-rate budget.
    pub max_request_rate: Option<Rate>,
    pub state: StatePolicy,
    /// Runs once per scenario per run.
    pub setup: Vec<Step>,
    /// Runs once per scenario per run.
    pub teardown: Vec<Step>,
    /// Runs once per virtual user.
    pub vu_setup: Vec<Step>,
    /// Runs once per virtual user.
    pub vu_teardown: Vec<Step>,
    pub steps: Vec<Step>,
}

/// What a virtual user keeps between iterations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatePolicy {
    /// Variables and cookies carry over from one iteration to the next.
    Persist,
    /// Variables and cookies return to how they were after `vu_setup`.
    ResetEachIteration,
}

/// A checked, immutable workload plan.
///
/// [`WorkloadPlan::new`] is the only way to get one. It is read-only after
/// construction, and `Send + Sync + 'static`, so the engine shares it through
/// an `Arc`.
///
/// ```
/// use vush_core::plan::{PlanDraft, PlanError, WorkloadPlan};
///
/// fn build(draft: PlanDraft) -> Result<WorkloadPlan, Vec<PlanError>> {
///     WorkloadPlan::new(draft)
/// }
/// ```
///
/// A plan can't be built any other way:
///
/// ```compile_fail
/// use vush_core::plan::{PlanDraft, WorkloadPlan};
///
/// fn forge(draft: PlanDraft) -> WorkloadPlan {
///     WorkloadPlan { draft }
/// }
/// ```
#[derive(Debug, PartialEq)]
pub struct WorkloadPlan {
    draft: PlanDraft,
}

const _: () = {
    const fn assert_shareable<T: Send + Sync + 'static>() {}
    assert_shareable::<WorkloadPlan>();
};

impl WorkloadPlan {
    /// Checks a draft and, if it is sound, turns it into a plan.
    ///
    /// Every problem is reported, not just the first. Problems come in
    /// document order: file-level `setup` and `teardown`, then `scenarios`,
    /// and for each scenario its `name`, `load`, `setup`, `teardown`,
    /// `vu_setup`, `vu_teardown` and `steps`. Inside an `if`, `then` comes
    /// before `else`.
    ///
    /// The walk over nested steps is recursive, and a stack overflow can't be
    /// caught, so callers bound the nesting depth before building a draft.
    pub fn new(draft: PlanDraft) -> Result<WorkloadPlan, Vec<PlanError>> {
        let mut errors = Vec::new();
        check_steps(&draft.setup, "setup", &mut errors);
        check_steps(&draft.teardown, "teardown", &mut errors);
        if draft.scenarios.is_empty() {
            errors.push(PlanError::new("scenarios", PlanErrorKind::NoScenarios));
        }
        let mut seen_names = HashSet::new();
        for (index, scenario) in draft.scenarios.iter().enumerate() {
            check_scenario(scenario, index, &mut seen_names, &mut errors);
        }
        if errors.is_empty() {
            Ok(WorkloadPlan { draft })
        } else {
            Err(errors)
        }
    }

    pub fn name(&self) -> &str {
        &self.draft.name
    }

    pub fn base_url(&self) -> Option<&Template> {
        self.draft.base_url.as_ref()
    }

    pub fn http(&self) -> &HttpSettings {
        &self.draft.http
    }

    pub fn limits(&self) -> &ResourceLimits {
        &self.draft.limits
    }

    /// The request-rate budget across all scenarios.
    pub fn max_request_rate(&self) -> Option<Rate> {
        self.draft.max_request_rate
    }

    pub fn setup(&self) -> &[Step] {
        &self.draft.setup
    }

    pub fn teardown(&self) -> &[Step] {
        &self.draft.teardown
    }

    pub fn thresholds(&self) -> &[Threshold] {
        &self.draft.thresholds
    }

    pub fn scenarios(&self) -> &[Scenario] {
        &self.draft.scenarios
    }
}

/// One problem found in a draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanError {
    /// Where the problem is, using the file's own key names, such as
    /// `scenarios[1].steps[0].repeat.steps`.
    pub path: String,
    pub kind: PlanErrorKind,
}

/// What is wrong at a [`PlanError`]'s path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanErrorKind {
    /// `scenarios` is empty.
    NoScenarios,
    /// A later scenario reuses this name.
    DuplicateScenarioName(String),
    /// A step list that must hold at least one step is empty.
    NoSteps,
    /// A ramping load has no stages.
    NoStages,
}

impl PlanError {
    fn new(path: impl Into<String>, kind: PlanErrorKind) -> Self {
        Self {
            path: path.into(),
            kind,
        }
    }
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.kind)
    }
}

impl std::error::Error for PlanError {}

impl fmt::Display for PlanErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoScenarios => f.write_str("at least one scenario is required"),
            Self::DuplicateScenarioName(name) => {
                write!(
                    f,
                    "scenario name {name:?} is already used by an earlier scenario"
                )
            }
            Self::NoSteps => f.write_str("at least one step is required"),
            Self::NoStages => f.write_str("a ramping load needs at least one stage"),
        }
    }
}

/// Checks one scenario, whose position in `scenarios` is `index`.
fn check_scenario<'a>(
    scenario: &'a Scenario,
    index: usize,
    seen_names: &mut HashSet<&'a str>,
    errors: &mut Vec<PlanError>,
) {
    let path = format!("scenarios[{index}]");
    if !seen_names.insert(scenario.name.as_str()) {
        errors.push(PlanError::new(
            format!("{path}.name"),
            PlanErrorKind::DuplicateScenarioName(scenario.name.clone()),
        ));
    }
    check_load(&scenario.load, &format!("{path}.load"), errors);
    check_steps(&scenario.setup, &format!("{path}.setup"), errors);
    check_steps(&scenario.teardown, &format!("{path}.teardown"), errors);
    check_steps(&scenario.vu_setup, &format!("{path}.vu_setup"), errors);
    check_steps(
        &scenario.vu_teardown,
        &format!("{path}.vu_teardown"),
        errors,
    );
    let steps_path = format!("{path}.steps");
    if scenario.steps.is_empty() {
        errors.push(PlanError::new(&steps_path, PlanErrorKind::NoSteps));
    }
    check_steps(&scenario.steps, &steps_path, errors);
}

/// Checks a load model, whose own path is `path`.
fn check_load(load: &LoadModel, path: &str, errors: &mut Vec<PlanError>) {
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
        errors.push(PlanError::new(
            format!("{path}.stages"),
            PlanErrorKind::NoStages,
        ));
    }
}

/// Checks every step in a list, and the lists nested inside them.
///
/// `path` is the list's own path, such as `scenarios[0].steps`. This does not
/// check that the list itself is non-empty: the caller knows whether it may be
/// empty, and reports [`PlanErrorKind::NoSteps`] when it may not.
fn check_steps(steps: &[Step], path: &str, errors: &mut Vec<PlanError>) {
    for (index, step) in steps.iter().enumerate() {
        let step_path = format!("{path}[{index}]");
        match step {
            Step::Request(_) | Step::Wait(_) => {}
            Step::If(step) => {
                let then_path = format!("{step_path}.if.then");
                if step.then.is_empty() {
                    errors.push(PlanError::new(&then_path, PlanErrorKind::NoSteps));
                }
                check_steps(&step.then, &then_path, errors);
                check_steps(&step.otherwise, &format!("{step_path}.if.else"), errors);
            }
            Step::Repeat(step) => {
                let steps_path = format!("{step_path}.repeat.steps");
                if step.steps.is_empty() {
                    errors.push(PlanError::new(&steps_path, PlanErrorKind::NoSteps));
                }
                check_steps(&step.steps, &steps_path, errors);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::time::Duration;

    use super::super::load::{ClosedStop, QueuePolicy};
    use super::super::step::{Comparison, Condition, IfStep, RepeatStep};
    use super::super::template::VarRef;
    use super::*;

    fn wait() -> Step {
        Step::Wait(Duration::from_secs(1))
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
                var: VarRef::Vu("x".to_string()),
                op: Comparison::Exists(true),
            },
            then,
            otherwise,
        })
    }

    fn scenario(name: &str) -> Scenario {
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

    fn draft(scenarios: Vec<Scenario>) -> PlanDraft {
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

    fn errors(draft: PlanDraft) -> Vec<(String, PlanErrorKind)> {
        WorkloadPlan::new(draft)
            .unwrap_err()
            .into_iter()
            .map(|e| (e.path, e.kind))
            .collect()
    }

    #[test]
    fn valid_plan_builds_and_accessors_return_inputs() {
        let plan = WorkloadPlan::new(draft(vec![scenario("a"), scenario("b")])).unwrap();
        assert_eq!(plan.name(), "test");
        assert_eq!(plan.scenarios()[1].name, "b");
        assert_eq!(plan.http(), &HttpSettings::default());
    }

    #[test]
    fn no_scenarios_is_reported() {
        assert_eq!(
            errors(draft(vec![])),
            [("scenarios".to_string(), PlanErrorKind::NoScenarios)]
        );
    }

    #[test]
    fn duplicate_scenario_name_is_reported_at_later_occurrence() {
        let d = draft(vec![scenario("a"), scenario("b"), scenario("a")]);
        assert_eq!(
            errors(d),
            [(
                "scenarios[2].name".to_string(),
                PlanErrorKind::DuplicateScenarioName("a".to_string())
            )]
        );
    }

    #[test]
    fn empty_scenario_steps_are_reported() {
        let mut s = scenario("a");
        s.steps.clear();
        assert_eq!(
            errors(draft(vec![s])),
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
            errors(draft(vec![s])),
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
            errors(d),
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
                errors(draft(vec![s])),
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
        assert!(WorkloadPlan::new(draft(vec![s])).is_ok());
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
            errors(d),
            [
                ("setup[0].repeat.steps".to_string(), PlanErrorKind::NoSteps),
                (
                    "scenarios[0].load.stages".to_string(),
                    PlanErrorKind::NoStages
                ),
                ("scenarios[0].steps".to_string(), PlanErrorKind::NoSteps),
                (
                    "scenarios[1].name".to_string(),
                    PlanErrorKind::DuplicateScenarioName("a".to_string())
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
        let paths: Vec<String> = errors(d).into_iter().map(|(path, _)| path).collect();
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
        assert_eq!(errors(draft(vec![s])), [(want, PlanErrorKind::NoSteps)]);
    }

    #[test]
    fn plan_error_display_leads_with_path() {
        let error = PlanError {
            path: "scenarios".to_string(),
            kind: PlanErrorKind::NoScenarios,
        };
        assert!(error.to_string().starts_with("scenarios: "));
    }
}
