//! The errors a plan check reports, and the paths that locate them.

use std::fmt;
use std::num::NonZeroUsize;

/// The most errors a plan check records. When there are more, the list ends
/// with one [`PlanErrorKind::TooManyErrors`] instead.
pub const MAX_PLAN_ERRORS: usize = 100;

/// The most bytes of a key a [`PlanPath`] keeps. A longer key is stored as a
/// [`PathSegment::TruncatedKey`].
pub const MAX_PATH_KEY_BYTES: usize = 256;

/// One step of a [`PlanPath`]: a key in a map, or a position in a list.
///
/// Build a key segment with [`PathSegment::for_key`], which cuts long keys,
/// and test it against a key with [`PathSegment::matches_key`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSegment {
    Key(String),
    /// A key longer than [`MAX_PATH_KEY_BYTES`]. `len` and `fingerprint` tell
    /// it apart from other long keys that start with the same prefix.
    TruncatedKey {
        /// The key's longest prefix of at most [`MAX_PATH_KEY_BYTES`] bytes
        /// that ends on a character boundary.
        prefix: String,
        /// The whole key's length in bytes.
        len: usize,
        /// A hash of the whole key.
        fingerprint: u64,
    },
    Index(usize),
}

impl PathSegment {
    /// A segment for a map key, cut to a [`PathSegment::TruncatedKey`] when
    /// it is longer than [`MAX_PATH_KEY_BYTES`].
    pub fn for_key(key: &str) -> Self {
        if key.len() <= MAX_PATH_KEY_BYTES {
            return Self::Key(key.to_owned());
        }
        let end = key.floor_char_boundary(MAX_PATH_KEY_BYTES);
        // `end` is a character boundary inside the key, so the split can't fail.
        let prefix = key.split_at_checked(end).map_or("", |(prefix, _)| prefix);
        Self::TruncatedKey {
            prefix: prefix.to_owned(),
            len: key.len(),
            fingerprint: fingerprint(key),
        }
    }

    /// Whether this segment names `key`. An index names no key.
    ///
    /// A cut key matches a key with the same prefix, length and fingerprint,
    /// so two long keys that share a prefix are told apart.
    pub fn matches_key(&self, key: &str) -> bool {
        match self {
            Self::Key(own) => own == key,
            Self::TruncatedKey {
                prefix,
                len,
                fingerprint: own,
            } => key.len() == *len && key.starts_with(prefix.as_str()) && fingerprint(key) == *own,
            Self::Index(_) => false,
        }
    }
}

/// A 64-bit FNV-1a hash of `key`. It is fixed by this code, not by the
/// standard library, so every crate built from this source agrees on it.
fn fingerprint(key: &str) -> u64 {
    key.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Where a problem is, as the file's own key names and list positions, such
/// as `scenarios[1].steps[0].repeat.steps`.
///
/// Each key stays one segment, even one that holds a dot. A key longer than
/// [`MAX_PATH_KEY_BYTES`] is kept as a [`PathSegment::TruncatedKey`].
///
/// `Display` prints each key as written, apart from that cut, which it shows
/// as the prefix followed by `…`. Code that shows a path to people must
/// escape its keys, as `vush-config`'s `render_path` does.
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
                PathSegment::TruncatedKey { prefix, .. } if position == 0 => {
                    write!(f, "{prefix}…")?;
                }
                PathSegment::TruncatedKey { prefix, .. } => write!(f, ".{prefix}…")?,
                PathSegment::Index(index) => write!(f, "[{index}]")?,
            }
        }
        Ok(())
    }
}

/// One problem found in a draft.
///
/// `Display` writes the path as [`PlanPath`] does: keys as written, apart
/// from the cut at [`MAX_PATH_KEY_BYTES`]. Code that shows an error to people
/// must escape the path's keys, as `vush-config`'s `render_path` does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanError {
    /// Where the problem is.
    pub path: PlanPath,
    pub kind: PlanErrorKind,
}

/// Which kind of variable a reference reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarScope {
    /// A user's own variable, such as `token`.
    User,
    /// `vu.id` or `vu.iteration`.
    BuiltIn,
    /// A `scenario.` variable.
    Scenario,
    /// A `global.` variable.
    Global,
}

/// A place where some variables can't be read. A user's own lists (`vu_setup`,
/// `steps` and `vu_teardown`) read every scope, so they aren't listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadPlace {
    /// `base_url`, which reads only `env.` variables.
    BaseUrl,
    /// The file-level `setup`.
    FileSetup,
    /// The file-level `teardown`.
    FileTeardown,
    /// A scenario's `setup`.
    ScenarioSetup,
    /// A scenario's `teardown`.
    ScenarioTeardown,
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
    /// A number in a `json` body or a comparison is NaN or infinite.
    NonFiniteNumber,
    /// An `extract` name, or a name a reference holds, isn't
    /// `[A-Za-z_][A-Za-z0-9_]*`.
    InvalidVariableName,
    /// An `extract` name, or a reference's bare name, is `env`, `global`,
    /// `scenario` or `vu`.
    ReservedVariableName,
    /// A reference reads a scope that can't be read at its place.
    VariableNotReadable {
        scope: VarScope,
        place: ReadPlace,
        /// Another scope that sets the name and can be read here.
        hint: Option<VarScope>,
        /// The reference's 1-based position among the value's references,
        /// when the value holds two or more.
        position: Option<NonZeroUsize>,
    },
    /// A reference reads a name that no `extract` sets for its scope.
    VariableNotSet {
        scope: VarScope,
        /// Another scope that sets the name and can be read here.
        hint: Option<VarScope>,
        /// The reference's 1-based position among the value's references,
        /// when the value holds two or more.
        position: Option<NonZeroUsize>,
    },
    /// More than [`MAX_PLAN_ERRORS`] problems were found, and the rest were
    /// dropped. It ends the list, and its path is empty.
    TooManyErrors,
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
            Self::NonFiniteNumber => f.write_str("a number must be finite, not NaN or infinity"),
            Self::InvalidVariableName => {
                f.write_str("a variable name must match `[A-Za-z_][A-Za-z0-9_]*`")
            }
            Self::ReservedVariableName => f.write_str(
                "`env`, `global`, `scenario` and `vu` are reserved and can't be variable names",
            ),
            Self::VariableNotReadable {
                scope,
                place,
                hint,
                position,
            } => {
                let why = match place {
                    ReadPlace::BaseUrl => None,
                    ReadPlace::FileSetup => Some(
                        "the file-level setup runs once, before any scenario and outside any user",
                    ),
                    ReadPlace::FileTeardown => Some(
                        "the file-level teardown runs once, after every scenario and outside any user",
                    ),
                    ReadPlace::ScenarioSetup => {
                        Some("a scenario's setup runs once, before its users start")
                    }
                    ReadPlace::ScenarioTeardown => {
                        Some("a scenario's teardown runs once, after its users finish")
                    }
                };
                match why {
                    Some(why) => write!(f, "{} can't be read here, because {why}", what(*scope))?,
                    None => f.write_str(
                        "`base_url` can only read `env.` variables, because it's used before any setup runs",
                    )?,
                }
                hint_and_position(f, *hint, *position)
            }
            Self::VariableNotSet {
                scope,
                hint,
                position,
            } => {
                f.write_str(match scope {
                    VarScope::User => {
                        "no `extract` in this scenario's `vu_setup`, `steps` or `vu_teardown` sets this variable"
                    }
                    VarScope::Scenario => "no `extract` in this scenario's `setup` sets this variable",
                    VarScope::Global => "no `extract` in the file-level `setup` sets this variable",
                    // `vu.id` and `vu.iteration` are never reported as not set.
                    VarScope::BuiltIn => "no `extract` sets this variable",
                })?;
                hint_and_position(f, *hint, *position)
            }
            // Formatted from the constant, so the number can't drift from it.
            Self::TooManyErrors => write!(
                f,
                "too many problems; only the first {MAX_PLAN_ERRORS} are reported"
            ),
        }
    }
}

/// What a [`PlanErrorKind::VariableNotReadable`] says can't be read.
fn what(scope: VarScope) -> &'static str {
    match scope {
        VarScope::User => "a user's own variables",
        VarScope::BuiltIn => "`vu.id` and `vu.iteration`",
        VarScope::Scenario => "`scenario.` variables",
        VarScope::Global => "`global.` variables",
    }
}

/// Writes the hint and the position suffix that end a variable error.
fn hint_and_position(
    f: &mut fmt::Formatter<'_>,
    hint: Option<VarScope>,
    position: Option<NonZeroUsize>,
) -> fmt::Result {
    f.write_str(match hint {
        Some(VarScope::User) => "; it's set as a user's own variable, so read it without a prefix",
        Some(VarScope::Scenario) => {
            "; it's set in this scenario's hooks, so read it with `scenario.`"
        }
        Some(VarScope::Global) => "; it's set in the file-level hooks, so read it with `global.`",
        // `vu.id` and `vu.iteration` are never a hint.
        Some(VarScope::BuiltIn) | None => "",
    })?;
    match position {
        Some(n) => write!(f, " (reference {n} in this value)"),
        None => Ok(()),
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
        let long = |c: &str| c.repeat(MAX_PATH_KEY_BYTES + 1);
        let cut = PlanPath::new(vec![
            PathSegment::for_key(&long("a")),
            Index(0),
            PathSegment::for_key(&long("c")),
        ]);
        let prefix = |c: &str| c.repeat(MAX_PATH_KEY_BYTES);
        assert_eq!(
            cut.to_string(),
            format!("{}…[0].{}…", prefix("a"), prefix("c"))
        );
    }

    #[test]
    fn short_keys_stay_whole() {
        let key = "k".repeat(MAX_PATH_KEY_BYTES);
        assert_eq!(PathSegment::for_key(&key), PathSegment::Key(key.clone()));
        assert!(PathSegment::for_key(&key).matches_key(&key));
        assert!(!PathSegment::for_key(&key).matches_key("k"));
        assert!(!PathSegment::Index(0).matches_key("0"));
    }

    #[test]
    fn long_keys_are_cut_on_a_character_boundary() {
        // 'é' is two bytes, so byte 256 falls inside the 129th one.
        let key = format!("a{}", "é".repeat(200));
        let PathSegment::TruncatedKey { prefix, len, .. } = PathSegment::for_key(&key) else {
            panic!("a long key is cut");
        };
        assert_eq!(prefix, format!("a{}", "é".repeat(127)));
        assert_eq!(prefix.len(), MAX_PATH_KEY_BYTES - 1);
        assert_eq!(len, key.len());
    }

    #[test]
    fn a_cut_key_matches_only_its_own_key() {
        let shared = "x".repeat(MAX_PATH_KEY_BYTES);
        let first = format!("{shared}A");
        let second = format!("{shared}B");
        let segment = PathSegment::for_key(&second);
        assert!(segment.matches_key(&second));
        // Same prefix and length, different tail.
        assert!(!segment.matches_key(&first));
        // Same prefix, different length.
        assert!(!segment.matches_key(&format!("{second}C")));
        assert!(!segment.matches_key(&shared));
    }

    #[test]
    fn duplicate_scenario_names_have_fixed_text() {
        assert_eq!(
            PlanErrorKind::DuplicateScenarioName.to_string(),
            "this scenario name is already used by an earlier scenario"
        );
    }

    #[test]
    fn too_many_errors_has_fixed_text() {
        assert_eq!(
            PlanErrorKind::TooManyErrors.to_string(),
            "too many problems; only the first 100 are reported"
        );
    }

    #[test]
    fn non_finite_numbers_have_fixed_text() {
        // No file can reach this kind, because the readers reject NaN and
        // infinity, so `vush-config`'s secrets tests can't cover it. It holds
        // no data, so its text can't carry a value.
        assert_eq!(
            PlanErrorKind::NonFiniteNumber.to_string(),
            "a number must be finite, not NaN or infinity"
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
