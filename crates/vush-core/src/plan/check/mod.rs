//! The structural checks that run when a draft becomes a plan.
//!
//! The walk visits a draft in plan order and passes down a borrowed [`At`]
//! path, so nothing is allocated until a problem is recorded. Every problem
//! is recorded through [`push`], which keeps at most [`MAX_PLAN_ERRORS`].

use std::cmp::Ordering;
use std::collections::HashSet;
use std::time::Duration;

use super::error::{
    MAX_PATH_KEY_BYTES, MAX_PLAN_ERRORS, PathSegment, PlanError, PlanErrorKind, PlanPath,
};
use super::step::{
    Body, Comparison, Condition, JsonTemplate, RequestStep, RequestTarget, Scalar, Step,
};
use super::template::Template;
use super::threshold::{BoundOp, Threshold, ThresholdCheck};
use super::workload::{PlanDraft, Scenario};

use vars::{Place, Writers};

mod values;
mod vars;

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

    /// Builds the owned path. Only called when a problem is recorded. A key
    /// longer than [`MAX_PATH_KEY_BYTES`] is cut, so a huge key can't be
    /// copied into every error under it.
    pub(super) fn path(&self) -> PlanPath {
        let mut segments = Vec::new();
        let mut at = self;
        loop {
            match at {
                At::Root => break,
                At::Key(parent, key) => {
                    segments.push(key_segment(key));
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

/// The segment for a map key: the key itself, or, when it is longer than
/// [`MAX_PATH_KEY_BYTES`], its longest prefix that fits and ends on a
/// character boundary.
fn key_segment(key: &str) -> PathSegment {
    if key.len() <= MAX_PATH_KEY_BYTES {
        return PathSegment::Key(key.to_owned());
    }
    let end = key.floor_char_boundary(MAX_PATH_KEY_BYTES);
    // `end` is a character boundary inside the key, so the split can't fail.
    let prefix = key.split_at_checked(end).map_or("", |(prefix, _)| prefix);
    PathSegment::TruncatedKey(prefix.to_owned())
}

/// Records a problem at a place, keeping at most [`MAX_PLAN_ERRORS`] in the
/// order they are found. The first problem past that is recorded as one
/// [`PlanErrorKind::TooManyErrors`] with an empty path, which ends the list.
/// Later problems are dropped without building their paths.
pub(super) fn push(errors: &mut Vec<PlanError>, at: &At<'_>, kind: PlanErrorKind) {
    let error = match errors.len().cmp(&MAX_PLAN_ERRORS) {
        Ordering::Less => PlanError {
            path: at.path(),
            kind,
        },
        Ordering::Equal => PlanError {
            path: PlanPath::default(),
            kind: PlanErrorKind::TooManyErrors,
        },
        Ordering::Greater => return,
    };
    errors.push(error);
}

/// What the walk carries down: the `http.timeout` that requests inherit,
/// whether there is a `base_url`, where references are read, and the names
/// each list's `extract`s set.
#[derive(Clone, Copy)]
struct Scope<'w> {
    request_timeout: Duration,
    has_base_url: bool,
    place: Place,
    writers: &'w Writers<'w>,
}

/// Finds the problems in a draft, in plan order: `base_url`, file-level
/// `http`, `limits`, `setup` and `teardown`, then `thresholds`, then
/// `scenarios`. It keeps the first [`MAX_PLAN_ERRORS`], and ends a list that
/// was cut with one [`PlanErrorKind::TooManyErrors`].
pub(super) fn check(draft: &PlanDraft) -> Vec<PlanError> {
    let mut errors = Vec::new();
    let root = At::Root;
    let writers = Writers::collect(draft);
    let scope = Scope {
        request_timeout: draft.http.timeout,
        has_base_url: draft.base_url.is_some(),
        place: Place::FileSetup,
        writers: &writers,
    };
    if let Some(base_url) = &draft.base_url {
        vars::template(
            base_url,
            Place::BaseUrl,
            &writers,
            &root.key("base_url"),
            &mut errors,
        );
    }
    let names: HashSet<&str> = draft.scenarios.iter().map(|s| s.name.as_str()).collect();
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
    let teardown = Scope {
        place: Place::FileTeardown,
        ..scope
    };
    check_steps(
        &draft.teardown,
        &root.key("teardown"),
        teardown,
        &mut errors,
    );
    let thresholds = root.key("thresholds");
    for (index, threshold) in draft.thresholds.iter().enumerate() {
        check_threshold(threshold, &names, &thresholds.index(index), &mut errors);
    }
    let scenarios = root.key("scenarios");
    if draft.scenarios.is_empty() {
        push(&mut errors, &scenarios, PlanErrorKind::NoScenarios);
    }
    let mut seen_names = HashSet::new();
    for (index, scenario) in draft.scenarios.iter().enumerate() {
        check_scenario(
            draft,
            index,
            scenario,
            &scenarios.index(index),
            &mut seen_names,
            &writers,
            &mut errors,
        );
    }
    errors
}

/// Checks one threshold, whose own place is `at`.
fn check_threshold(
    threshold: &Threshold,
    names: &HashSet<&str>,
    at: &At<'_>,
    errors: &mut Vec<PlanError>,
) {
    if let ThresholdCheck::HttpRequestDuration { bound, .. } = &threshold.check {
        let key = match bound.op {
            BoundOp::Lt => "lt",
            BoundOp::Lte => "lte",
            BoundOp::Gt => "gt",
            BoundOp::Gte => "gte",
        };
        values::duration(bound.value, false, &at.key(key), errors);
    }
    values::threshold_scenario(threshold.scenario.as_deref(), names, at, errors);
}

/// Checks scenario number `index`, whose own place is `at`.
fn check_scenario<'a>(
    draft: &PlanDraft,
    index: usize,
    scenario: &'a Scenario,
    at: &At<'_>,
    seen_names: &mut HashSet<&'a str>,
    writers: &Writers<'_>,
    errors: &mut Vec<PlanError>,
) {
    let scope = Scope {
        request_timeout: scenario.http.timeout,
        has_base_url: draft.base_url.is_some(),
        place: Place::User(index),
        writers,
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
    let setup = Scope {
        place: Place::ScenarioSetup(index),
        ..scope
    };
    check_steps(&scenario.setup, &at.key("setup"), setup, errors);
    let teardown = Scope {
        place: Place::ScenarioTeardown(index),
        ..scope
    };
    check_steps(&scenario.teardown, &at.key("teardown"), teardown, errors);
    check_steps(&scenario.vu_setup, &at.key("vu_setup"), scope, errors);
    check_steps(&scenario.vu_teardown, &at.key("vu_teardown"), scope, errors);
    let steps = at.key("steps");
    if scenario.steps.is_empty() {
        push(errors, &steps, PlanErrorKind::NoSteps);
    }
    check_steps(&scenario.steps, &steps, scope, errors);
}

/// Checks a condition, whose own place is `at`.
fn check_condition(
    condition: &Condition,
    at: &At<'_>,
    scope: Scope<'_>,
    errors: &mut Vec<PlanError>,
) {
    match condition {
        Condition::Compare { var, op } => {
            vars::reference(
                var,
                scope.place,
                scope.writers,
                None,
                &at.key("var"),
                errors,
            );
            check_comparison(op, at, scope, errors);
        }
        Condition::All(list) => check_conditions(list, &at.key("all"), scope, errors),
        Condition::Any(list) => check_conditions(list, &at.key("any"), scope, errors),
        Condition::Not(inner) => check_condition(inner, &at.key("not"), scope, errors),
    }
}

fn check_conditions(
    list: &[Condition],
    at: &At<'_>,
    scope: Scope<'_>,
    errors: &mut Vec<PlanError>,
) {
    values::condition_list(list, at, errors);
    for (index, item) in list.iter().enumerate() {
        check_condition(item, &at.index(index), scope, errors);
    }
}

/// Checks the template in a comparison, if it has one, at its op key under
/// `at`: `equals`, `not_equals` or `contains`.
fn check_comparison(op: &Comparison, at: &At<'_>, scope: Scope<'_>, errors: &mut Vec<PlanError>) {
    let (key, template) = match op {
        Comparison::Equals(Scalar::String(template)) => ("equals", template),
        Comparison::NotEquals(Scalar::String(template)) => ("not_equals", template),
        Comparison::Contains(template) => ("contains", template),
        // Every variant is named, so a new one that holds a template can't
        // be skipped without a compile error here.
        Comparison::Equals(Scalar::Null | Scalar::Bool(_) | Scalar::Number(_))
        | Comparison::NotEquals(Scalar::Null | Scalar::Bool(_) | Scalar::Number(_))
        | Comparison::Exists(_)
        | Comparison::Gt(_)
        | Comparison::Gte(_)
        | Comparison::Lt(_)
        | Comparison::Lte(_) => return,
    };
    check_template(template, &at.key(key), scope, errors);
}

fn check_template(template: &Template, at: &At<'_>, scope: Scope<'_>, errors: &mut Vec<PlanError>) {
    vars::template(template, scope.place, scope.writers, at, errors);
}

/// Checks the values of a `query`, `headers` or `form` map, whose own place is
/// `at`.
fn check_pairs(
    pairs: &[(String, Template)],
    at: &At<'_>,
    scope: Scope<'_>,
    errors: &mut Vec<PlanError>,
) {
    for (name, template) in pairs {
        check_template(template, &at.key(name), scope, errors);
    }
}

/// Checks every string in a `json` body, whose own place is `at`.
fn check_json(json: &JsonTemplate, at: &At<'_>, scope: Scope<'_>, errors: &mut Vec<PlanError>) {
    match json {
        JsonTemplate::String(template) => check_template(template, at, scope, errors),
        JsonTemplate::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                check_json(item, &at.index(index), scope, errors);
            }
        }
        JsonTemplate::Object(members) => {
            for (key, value) in members {
                check_json(value, &at.key(key), scope, errors);
            }
        }
        JsonTemplate::Null | JsonTemplate::Bool(_) | JsonTemplate::Number(_) => {}
    }
}

/// Checks one request, whose own place is `at`, in plan order.
fn check_request(
    request: &RequestStep,
    at: &At<'_>,
    scope: Scope<'_>,
    errors: &mut Vec<PlanError>,
) {
    values::request_name(request.name.as_deref(), at, errors);
    values::target(&request.target, scope.has_base_url, at, errors);
    let (key, target) = match &request.target {
        RequestTarget::Path(template) => ("path", template),
        RequestTarget::Url(template) => ("url", template),
    };
    check_template(target, &at.key(key), scope, errors);
    check_pairs(&request.query, &at.key("query"), scope, errors);
    check_pairs(&request.headers, &at.key("headers"), scope, errors);
    match &request.body {
        Some(Body::Json(json)) => check_json(json, &at.key("json"), scope, errors),
        Some(Body::Text(template)) => check_template(template, &at.key("body"), scope, errors),
        Some(Body::Form(pairs)) => check_pairs(pairs, &at.key("form"), scope, errors),
        None => {}
    }
    values::inherited(
        request.timeout,
        scope.request_timeout,
        &at.key("timeout"),
        errors,
    );
    if let Some(retry) = &request.retry {
        values::retry(retry, &request.method, &at.key("retry"), errors);
    }
    let expect = at.key("expect");
    values::status(&request.expect.status, &expect.key("status"), errors);
    if let Some(template) = &request.expect.body_contains {
        check_template(template, &expect.key("body_contains"), scope, errors);
    }
    let json = expect.key("json");
    for (index, assertion) in request.expect.json.iter().enumerate() {
        check_comparison(&assertion.op, &json.index(index), scope, errors);
    }
    vars::extract_names(&request.extract, &at.key("extract"), errors);
}

/// Checks every step in a list, and the lists nested inside them.
///
/// `at` is the list's own place, such as `scenarios[0].steps`. This does not
/// check that the list itself is non-empty: the caller knows whether it may be
/// empty, and reports [`PlanErrorKind::NoSteps`] when it may not.
fn check_steps(steps: &[Step], at: &At<'_>, scope: Scope<'_>, errors: &mut Vec<PlanError>) {
    for (index, step) in steps.iter().enumerate() {
        let step_at = at.index(index);
        match step {
            Step::Request(request) => {
                check_request(request, &step_at.key("request"), scope, errors);
            }
            Step::Wait(value) => values::duration(*value, false, &step_at.key("wait"), errors),
            Step::If(step) => {
                let if_at = step_at.key("if");
                check_condition(&step.when, &if_at.key("when"), scope, errors);
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
    use super::super::step::{
        Comparison, Condition, Expect, IfStep, RepeatStep, StatusMatch, Step,
    };
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
        // `vu.id` can't be read outside a user, so this `if` reads `env.`.
        d.setup = vec![Step::If(IfStep {
            when: Condition::Compare {
                var: VarRef::Env("X".to_string()),
                op: Comparison::Exists(true),
            },
            then: vec![],
            otherwise: vec![],
        })];
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

    fn url() -> RequestTarget {
        RequestTarget::Url(tpl("http://localhost/"))
    }

    fn one_request(r: RequestStep) -> PlanDraft {
        let mut s = scenario("a");
        s.steps = vec![Step::Request(Box::new(r))];
        draft(vec![s])
    }

    fn at(path: &str, kind: PlanErrorKind) -> (String, PlanErrorKind) {
        (path.to_string(), kind)
    }

    const REQ: &str = "scenarios[0].steps[0].request";

    fn retrying(method: &str, attempts: u32, allow: bool) -> PlanDraft {
        let mut r = request(method, url());
        r.retry = Some(RetryPolicy {
            attempts,
            backoff: Duration::ZERO,
            allow_non_idempotent: allow,
        });
        one_request(r)
    }

    #[test]
    fn attempts_must_be_two_or_more() {
        assert!(errors(&retrying("GET", 2, false)).is_empty());
        for attempts in [1, 0] {
            assert_eq!(
                errors(&retrying("GET", attempts, false)),
                [at(
                    &format!("{REQ}.retry.attempts"),
                    PlanErrorKind::TooFewAttempts
                )]
            );
        }
    }

    #[test]
    fn retrying_a_non_idempotent_method_needs_the_opt_in() {
        for method in ["POST", "PATCH", "CONNECT", "PURGE", "M-SEARCH"] {
            assert_eq!(
                errors(&retrying(method, 2, false)),
                [at(&format!("{REQ}.retry"), PlanErrorKind::RetryNeedsOptIn)],
                "{method}"
            );
            assert!(errors(&retrying(method, 2, true)).is_empty(), "{method}");
        }
        for method in values::IDEMPOTENT {
            assert!(errors(&retrying(method, 2, false)).is_empty(), "{method}");
        }
    }

    fn with_status(status: StatusMatch) -> PlanDraft {
        let mut r = request("GET", url());
        r.expect = Expect {
            status,
            ..Expect::default()
        };
        one_request(r)
    }

    #[test]
    fn status_codes_must_be_from_100_to_599() {
        for ok in [
            StatusMatch::OneOf(vec![100]),
            StatusMatch::OneOf(vec![599]),
            StatusMatch::OneOf(vec![200, 304]),
            StatusMatch::AnySuccess,
        ] {
            assert!(errors(&with_status(ok)).is_empty());
        }
        let path = format!("{REQ}.expect.status");
        for bad in [vec![99], vec![600], vec![200, 0, 700]] {
            assert_eq!(
                errors(&with_status(StatusMatch::OneOf(bad))),
                [at(&path, PlanErrorKind::StatusOutOfRange)]
            );
        }
        assert_eq!(
            errors(&with_status(StatusMatch::OneOf(vec![]))),
            [at(&path, PlanErrorKind::EmptyStatus)]
        );
    }

    fn exists() -> Condition {
        Condition::Compare {
            var: VarRef::VuId,
            op: Comparison::Exists(true),
        }
    }

    fn when(condition: Condition) -> PlanDraft {
        let mut s = scenario("a");
        s.steps = vec![Step::If(IfStep {
            when: condition,
            then: vec![wait()],
            otherwise: vec![],
        })];
        draft(vec![s])
    }

    #[test]
    fn all_and_any_need_a_condition() {
        let base = "scenarios[0].steps[0].if.when";
        assert_eq!(
            errors(&when(Condition::All(vec![]))),
            [at(&format!("{base}.all"), PlanErrorKind::NoConditions)]
        );
        assert_eq!(
            errors(&when(Condition::Not(Box::new(Condition::Any(vec![]))))),
            [at(&format!("{base}.not.any"), PlanErrorKind::NoConditions)]
        );
        assert_eq!(
            errors(&when(Condition::All(vec![Condition::Any(vec![])]))),
            [at(
                &format!("{base}.all[0].any"),
                PlanErrorKind::NoConditions
            )]
        );
        assert!(errors(&when(Condition::All(vec![exists(), exists()]))).is_empty());
        assert!(errors(&when(Condition::Any(vec![exists()]))).is_empty());
    }

    #[test]
    fn request_names_cannot_hold_template_syntax() {
        let named = |name: Option<&str>| {
            let mut r = request("GET", url());
            r.name = name.map(str::to_string);
            one_request(r)
        };
        for bad in ["a${b}", "$${b}"] {
            assert_eq!(
                errors(&named(Some(bad))),
                [at(&format!("{REQ}.name"), PlanErrorKind::TemplateInName)]
            );
        }
        for ok in [Some("a$b"), Some("a{b}"), Some("price $5"), None] {
            assert!(errors(&named(ok)).is_empty());
        }
    }

    #[test]
    fn a_path_needs_a_base_url() {
        let path = || RequestTarget::Path(tpl("/x"));
        let mut s = scenario("a");
        s.steps = vec![Step::Request(Box::new(request("GET", path())))];
        let mut d = draft(vec![s]);
        d.setup = vec![Step::Request(Box::new(request("GET", path())))];
        assert_eq!(
            errors(&d),
            [
                at("setup[0].request.path", PlanErrorKind::PathNeedsBaseUrl),
                at(
                    "scenarios[0].steps[0].request.path",
                    PlanErrorKind::PathNeedsBaseUrl
                ),
            ]
        );
        d.base_url = Some(tpl("http://localhost"));
        assert!(errors(&d).is_empty());
        assert!(errors(&one_request(request("GET", url()))).is_empty());
    }

    #[test]
    fn thresholds_must_name_a_scenario_in_the_file() {
        let with = |scenario_name: Option<&str>| {
            let mut d = draft(vec![scenario("checkout")]);
            d.thresholds = vec![Threshold {
                scenario: scenario_name.map(str::to_string),
                check: ThresholdCheck::HttpRequestDuration {
                    stat: PercentileStat::P95,
                    bound: Bound {
                        op: BoundOp::Lt,
                        value: Duration::from_secs(1),
                    },
                },
            }];
            d
        };
        assert!(errors(&with(Some("checkout"))).is_empty());
        assert!(errors(&with(None)).is_empty());
        for bad in ["Checkout", "missing"] {
            assert_eq!(
                errors(&with(Some(bad))),
                [at("thresholds[0].scenario", PlanErrorKind::UnknownScenario)]
            );
        }
    }

    #[test]
    fn request_errors_come_in_plan_order() {
        let mut r = request("POST", RequestTarget::Path(tpl("/x")));
        r.name = Some("${x}".to_string());
        r.timeout = Duration::ZERO;
        r.retry = Some(RetryPolicy {
            attempts: 1,
            backoff: MAX_DURATION + MS,
            allow_non_idempotent: false,
        });
        r.expect.status = StatusMatch::OneOf(vec![]);
        let found: Vec<String> = errors(&one_request(r)).into_iter().map(|e| e.0).collect();
        let want: Vec<String> = [
            "name",
            "path",
            "timeout",
            "retry.attempts",
            "retry.backoff",
            "retry",
            "expect.status",
        ]
        .iter()
        .map(|k| format!("{REQ}.{k}"))
        .collect();
        assert_eq!(found, want);
    }

    #[test]
    fn errors_stop_at_the_cap() {
        let empty_repeats = |count: usize| {
            let mut s = scenario("a");
            s.steps = (0..count).map(|_| repeat(vec![])).collect();
            draft(vec![s])
        };
        let found = check(&empty_repeats(150));
        assert_eq!(found.len(), MAX_PLAN_ERRORS + 1);
        for (index, error) in found.iter().take(MAX_PLAN_ERRORS).enumerate() {
            assert_eq!(
                error.path.to_string(),
                format!("scenarios[0].steps[{index}].repeat.steps")
            );
            assert_eq!(error.kind, PlanErrorKind::NoSteps);
        }
        assert_eq!(
            found.last(),
            Some(&PlanError {
                path: PlanPath::default(),
                kind: PlanErrorKind::TooManyErrors,
            })
        );

        let found = check(&empty_repeats(MAX_PLAN_ERRORS));
        assert_eq!(found.len(), MAX_PLAN_ERRORS);
        assert!(found.iter().all(|e| e.kind == PlanErrorKind::NoSteps));
    }

    #[test]
    fn long_keys_are_cut_in_paths() {
        // The last path segment of the one error a JSON-body key over an unset
        // `${nope}` gives.
        let last_segment = |key: &str| {
            let mut r = request("POST", url());
            r.body = Some(Body::Json(JsonTemplate::Object(vec![(
                key.to_string(),
                JsonTemplate::String(tpl("${nope}")),
            )])));
            let found = check(&one_request(r));
            assert_eq!(found.len(), 1, "{found:?}");
            let path = &found[0].path;
            (path.segments().last().cloned(), path.to_string())
        };

        let (segment, text) = last_segment(&"é".repeat(1000));
        let prefix = "é".repeat(128);
        assert_eq!(prefix.len(), MAX_PATH_KEY_BYTES);
        assert_eq!(segment, Some(PathSegment::TruncatedKey(prefix.clone())));
        assert!(text.ends_with('…'), "{text}");
        assert_eq!(text, format!("{REQ}.json.{prefix}…"));

        // Byte 256 falls inside an `é`, so the cut ends one byte earlier.
        let (segment, _) = last_segment(&format!("a{}", "é".repeat(500)));
        let prefix = format!("a{}", "é".repeat(127));
        assert_eq!(prefix.len(), MAX_PATH_KEY_BYTES - 1);
        assert_eq!(segment, Some(PathSegment::TruncatedKey(prefix)));

        let fits = "k".repeat(MAX_PATH_KEY_BYTES);
        assert_eq!(last_segment(&fits).0, Some(PathSegment::Key(fits)));
        let (segment, _) = last_segment(&"k".repeat(MAX_PATH_KEY_BYTES + 1));
        assert_eq!(
            segment,
            Some(PathSegment::TruncatedKey("k".repeat(MAX_PATH_KEY_BYTES)))
        );
    }
}
