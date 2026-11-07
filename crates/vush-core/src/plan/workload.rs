//! The workload plan and the one checked constructor that builds it.

use super::check;
use super::error::PlanError;
use super::load::LoadModel;
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
    /// Every problem is reported, not just the first, up to
    /// [`MAX_PLAN_ERRORS`]. When there are more, the rest are dropped and one
    /// [`PlanErrorKind::TooManyErrors`] entry ends the list. Problems come in
    /// plan order. At file level that is `base_url`, `http`, `limits`,
    /// `setup`, `teardown`, `thresholds` and `scenarios`. In each scenario it
    /// is `name`, `load`, `http`, `limits`, `setup`, `teardown`, `vu_setup`,
    /// `vu_teardown` and `steps`. Inside an `if`, `when` comes before `then`
    /// and `then` before `else`.
    ///
    /// A path key longer than [`MAX_PATH_KEY_BYTES`] is cut to a
    /// [`PathSegment::TruncatedKey`].
    ///
    /// The walk over nested steps is recursive, and a stack overflow can't be
    /// caught, so callers bound the nesting depth before building a draft.
    ///
    /// [`MAX_PLAN_ERRORS`]: crate::plan::MAX_PLAN_ERRORS
    /// [`PlanErrorKind::TooManyErrors`]: crate::plan::PlanErrorKind::TooManyErrors
    /// [`MAX_PATH_KEY_BYTES`]: crate::plan::MAX_PATH_KEY_BYTES
    /// [`PathSegment::TruncatedKey`]: crate::plan::PathSegment::TruncatedKey
    pub fn new(draft: PlanDraft) -> Result<WorkloadPlan, Vec<PlanError>> {
        let errors = check::check(&draft);
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

#[cfg(test)]
mod tests {
    use super::super::check::testing::{draft, scenario};
    use super::*;

    #[test]
    fn valid_plan_builds_and_accessors_return_inputs() {
        let plan = WorkloadPlan::new(draft(vec![scenario("a"), scenario("b")])).unwrap();
        assert_eq!(plan.name(), "test");
        assert_eq!(plan.scenarios()[1].name, "b");
        assert_eq!(plan.http(), &HttpSettings::default());
    }
}
