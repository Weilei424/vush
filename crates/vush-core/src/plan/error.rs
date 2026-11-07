//! The errors a plan check reports, and the paths that locate them.

use std::fmt;

/// One step of a [`PlanPath`]: a key in a map, or a position in a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSegment {
    Key(String),
    Index(usize),
}

/// Where a problem is, as the file's own key names and list positions, such
/// as `scenarios[1].steps[0].repeat.steps`.
///
/// Keys are kept whole, so a key that holds a dot stays one segment.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlanPath(Vec<PathSegment>);

impl PlanPath {
    pub fn new(segments: Vec<PathSegment>) -> Self {
        Self(segments)
    }

    pub fn segments(&self) -> &[PathSegment] {
        &self.0
    }
}

impl fmt::Display for PlanPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (position, segment) in self.0.iter().enumerate() {
            match segment {
                PathSegment::Key(key) if position == 0 => f.write_str(key)?,
                PathSegment::Key(key) => write!(f, ".{key}")?,
                PathSegment::Index(index) => write!(f, "[{index}]")?,
            }
        }
        Ok(())
    }
}

/// One problem found in a draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanError {
    /// Where the problem is.
    pub path: PlanPath,
    pub kind: PlanErrorKind,
}

/// What is wrong at a [`PlanError`]'s path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanErrorKind {
    /// `scenarios` is empty.
    NoScenarios,
    /// A later scenario reuses an earlier scenario's name.
    DuplicateScenarioName,
    /// A step list that must hold at least one step is empty.
    NoSteps,
    /// A ramping load has no stages.
    NoStages,
    /// A duration is longer than the 30-day ceiling.
    DurationTooLong,
    /// A duration that must be positive is zero.
    ZeroDuration,
    /// A rate that must be positive is zero.
    ZeroRate,
    /// A ramping load never rises above zero.
    IdleRamp,
    /// A ramping load's stages last no time in total.
    ZeroLengthRamp,
    /// `retry.attempts` is below 2.
    TooFewAttempts,
    /// A non-idempotent method is retried without `allow_non_idempotent`.
    RetryNeedsOptIn,
    /// A status code is outside 100 to 599.
    StatusOutOfRange,
    /// `status` lists no codes.
    EmptyStatus,
    /// An `all` or `any` list holds no condition.
    NoConditions,
    /// A request name holds `${`.
    TemplateInName,
    /// A `path` target is used with no `base_url`.
    PathNeedsBaseUrl,
    /// A threshold names a scenario the file does not have.
    UnknownScenario,
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
            Self::DuplicateScenarioName => {
                f.write_str("this scenario name is already used by an earlier scenario")
            }
            Self::NoSteps => f.write_str("at least one step is required"),
            Self::NoStages => f.write_str("a ramping load needs at least one stage"),
            Self::DurationTooLong => f.write_str("a duration can be at most 720h (30 days)"),
            Self::ZeroDuration => f.write_str("this duration must be longer than zero"),
            Self::ZeroRate => f.write_str("this rate must be above zero"),
            Self::IdleRamp => {
                f.write_str("a ramping load must be above zero at its start or at some stage")
            }
            Self::ZeroLengthRamp => {
                f.write_str("a ramping load's stages must last longer than zero in total")
            }
            Self::TooFewAttempts => {
                f.write_str("`attempts` must be 2 or more, because it counts the first attempt")
            }
            Self::RetryNeedsOptIn => f.write_str(
                "retrying a method other than GET, HEAD, PUT, DELETE, OPTIONS or TRACE needs `allow_non_idempotent: true`",
            ),
            Self::StatusOutOfRange => f.write_str("status codes must be from 100 to 599"),
            Self::EmptyStatus => f.write_str(
                "`status` needs at least one code; leave it out to accept any 2xx or 3xx",
            ),
            Self::NoConditions => f.write_str("at least one condition is required"),
            Self::TemplateInName => {
                f.write_str("a request name is a fixed metrics label, so it can't contain `${`")
            }
            Self::PathNeedsBaseUrl => f.write_str(
                "a `path` needs a `base_url` at the top of the file; use `url` for a full address",
            ),
            Self::UnknownScenario => f.write_str("no scenario has this name"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_path_displays_like_compile_paths() {
        use PathSegment::{Index, Key};
        let path = PlanPath::new(vec![
            Key("scenarios".into()),
            Index(1),
            Key("steps".into()),
            Index(0),
            Key("repeat".into()),
            Key("steps".into()),
        ]);
        assert_eq!(path.to_string(), "scenarios[1].steps[0].repeat.steps");
        assert_eq!(PlanPath::default().to_string(), "");
        let dotted = PlanPath::new(vec![Key("a.b".into())]);
        assert_eq!(dotted.segments().len(), 1);
        assert_eq!(dotted.to_string(), "a.b");
    }

    #[test]
    fn duplicate_scenario_names_have_fixed_text() {
        assert_eq!(
            PlanErrorKind::DuplicateScenarioName.to_string(),
            "this scenario name is already used by an earlier scenario"
        );
    }

    #[test]
    fn plan_error_display_leads_with_path() {
        let error = PlanError {
            path: PlanPath::new(vec![PathSegment::Key("scenarios".to_string())]),
            kind: PlanErrorKind::NoScenarios,
        };
        assert!(error.to_string().starts_with("scenarios: "));
    }
}
