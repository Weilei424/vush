//! Checks on variables: where each reference reads, and the names `extract`
//! writes.
//!
//! An `extract` writes the scope of the list it runs in: the file-level hooks
//! write `global.` names, a scenario's `setup` and `teardown` write its
//! `scenario.` names, and its `vu_setup`, `steps` and `vu_teardown` write the
//! user's own names. A reference is checked for scope and name only: where in
//! a list the `extract` is, and whether its step is optional, are left to the
//! run.

use std::collections::HashSet;
use std::num::NonZeroUsize;

use super::super::error::{PlanError, PlanErrorKind, ReadPlace, VarScope};
use super::super::step::{Extraction, Step};
use super::super::template::{RESERVED, Template, TemplatePart, VarRef, is_name};
use super::super::workload::PlanDraft;
use super::{At, push};

/// Where a reference is read. The `usize` is the scenario's index.
#[derive(Clone, Copy)]
pub(super) enum Place {
    BaseUrl,
    FileSetup,
    FileTeardown,
    ScenarioSetup(usize),
    ScenarioTeardown(usize),
    /// A scenario's `vu_setup`, `steps` and `vu_teardown`.
    User(usize),
}

impl Place {
    /// How an error names this place, or `None` for a user's lists, which
    /// read every scope.
    fn read_place(self) -> Option<ReadPlace> {
        match self {
            Place::BaseUrl => Some(ReadPlace::BaseUrl),
            Place::FileSetup => Some(ReadPlace::FileSetup),
            Place::FileTeardown => Some(ReadPlace::FileTeardown),
            Place::ScenarioSetup(_) => Some(ReadPlace::ScenarioSetup),
            Place::ScenarioTeardown(_) => Some(ReadPlace::ScenarioTeardown),
            Place::User(_) => None,
        }
    }

    /// Whether a reference here can read `scope`. `env.` is readable
    /// everywhere, so it isn't asked about.
    fn reads(self, scope: VarScope) -> bool {
        match self {
            Place::BaseUrl => false,
            Place::FileSetup | Place::FileTeardown => scope == VarScope::Global,
            Place::ScenarioSetup(_) | Place::ScenarioTeardown(_) => {
                matches!(scope, VarScope::Scenario | VarScope::Global)
            }
            Place::User(_) => true,
        }
    }
}

/// The valid names each list's `extract`s write, at any depth.
pub(super) struct Writers<'d> {
    file_setup: HashSet<&'d str>,
    file_teardown: HashSet<&'d str>,
    /// One per scenario, in order.
    scenarios: Vec<ScenarioWriters<'d>>,
}

struct ScenarioWriters<'d> {
    setup: HashSet<&'d str>,
    teardown: HashSet<&'d str>,
    /// `vu_setup`, `steps` and `vu_teardown` together.
    user: HashSet<&'d str>,
}

impl<'d> Writers<'d> {
    /// Collects the names every list writes. Names that are invalid or
    /// reserved are skipped: they are reported where they are written.
    pub(super) fn collect(draft: &'d PlanDraft) -> Writers<'d> {
        Writers {
            file_setup: written(&[&draft.setup]),
            file_teardown: written(&[&draft.teardown]),
            scenarios: draft
                .scenarios
                .iter()
                .map(|scenario| ScenarioWriters {
                    setup: written(&[&scenario.setup]),
                    teardown: written(&[&scenario.teardown]),
                    user: written(&[&scenario.vu_setup, &scenario.steps, &scenario.vu_teardown]),
                })
                .collect(),
        }
    }

    /// Whether `name` is set for `scope`, as a read at `place` sees it. A
    /// scope that `place` can't read sees no names.
    fn sets(&self, scope: VarScope, place: Place, name: &str) -> bool {
        let scenario = |index: usize| self.scenarios.get(index);
        match (scope, place) {
            (VarScope::Global, Place::FileTeardown) => {
                self.file_setup.contains(name) || self.file_teardown.contains(name)
            }
            (
                VarScope::Global,
                Place::FileSetup
                | Place::ScenarioSetup(_)
                | Place::ScenarioTeardown(_)
                | Place::User(_),
            ) => self.file_setup.contains(name),
            (VarScope::Scenario, Place::ScenarioSetup(index) | Place::User(index)) => {
                scenario(index).is_some_and(|s| s.setup.contains(name))
            }
            (VarScope::Scenario, Place::ScenarioTeardown(index)) => {
                scenario(index).is_some_and(|s| s.setup.contains(name) || s.teardown.contains(name))
            }
            (VarScope::User, Place::User(index)) => {
                scenario(index).is_some_and(|s| s.user.contains(name))
            }
            _ => false,
        }
    }
}

/// The valid names the `extract`s in `lists` write, at any depth.
fn written<'d>(lists: &[&'d [Step]]) -> HashSet<&'d str> {
    let mut names = HashSet::new();
    for list in lists {
        add_written(list, &mut names);
    }
    names
}

fn add_written<'d>(steps: &'d [Step], names: &mut HashSet<&'d str>) {
    for step in steps {
        match step {
            Step::Request(request) => names.extend(
                request
                    .extract
                    .iter()
                    .map(|extraction| extraction.var.as_str())
                    .filter(|name| is_name(name) && !RESERVED.contains(name)),
            ),
            Step::Wait(_) => {}
            Step::If(step) => {
                add_written(&step.then, names);
                add_written(&step.otherwise, names);
            }
            Step::Repeat(step) => add_written(&step.steps, names),
        }
    }
}

/// Checks each `${...}` reference in a template at `place`. The template's
/// own place is `at`. `$${` escapes are text, so they aren't checked.
pub(super) fn template(
    template: &Template,
    place: Place,
    writers: &Writers<'_>,
    at: &At<'_>,
    errors: &mut Vec<PlanError>,
) {
    let references = || {
        template.parts().iter().filter_map(|part| match part {
            TemplatePart::Var(var) => Some(var),
            TemplatePart::Text(_) => None,
        })
    };
    // Positions are given only when there is more than one reference.
    let numbered = references().nth(1).is_some();
    for (index, var) in references().enumerate() {
        let position = numbered.then(|| NonZeroUsize::MIN.saturating_add(index));
        reference(var, place, writers, position, at, errors);
    }
}

/// Checks one reference read at `place`. `position` is its 1-based place
/// among its template's references, if the template has more than one; a
/// `var:` field passes `None`.
pub(super) fn reference(
    var: &VarRef,
    place: Place,
    writers: &Writers<'_>,
    position: Option<NonZeroUsize>,
    at: &At<'_>,
    errors: &mut Vec<PlanError>,
) {
    let (scope, name) = match var {
        VarRef::Env(_) => return,
        VarRef::VuId | VarRef::VuIteration => (VarScope::BuiltIn, None),
        VarRef::Vu(name) => (VarScope::User, Some(name.as_str())),
        VarRef::Scenario(name) => (VarScope::Scenario, Some(name.as_str())),
        VarRef::Global(name) => (VarScope::Global, Some(name.as_str())),
    };
    // Another scope that is readable here and sets the name, so reading it
    // there would fix the error. The built-ins have no name, so no hint.
    let hint = || {
        let name = name?;
        [VarScope::User, VarScope::Scenario, VarScope::Global]
            .into_iter()
            .find(|&other| other != scope && place.reads(other) && writers.sets(other, place, name))
    };
    if let Some(read_place) = place.read_place()
        && !place.reads(scope)
    {
        let kind = PlanErrorKind::VariableNotReadable {
            scope,
            place: read_place,
            hint: hint(),
            position,
        };
        push(errors, at, kind);
    } else if let Some(name) = name
        && !writers.sets(scope, place, name)
    {
        let kind = PlanErrorKind::VariableNotSet {
            scope,
            hint: hint(),
            position,
        };
        push(errors, at, kind);
    }
}

/// Checks that each `extract` name is a variable name and not reserved. `at`
/// is the `extract` map; problems are reported at the name.
pub(super) fn extract_names(extract: &[Extraction], at: &At<'_>, errors: &mut Vec<PlanError>) {
    for extraction in extract {
        let name = extraction.var.as_str();
        let kind = if !is_name(name) {
            PlanErrorKind::InvalidVariableName
        } else if RESERVED.contains(&name) {
            PlanErrorKind::ReservedVariableName
        } else {
            continue;
        };
        push(errors, &at.key(name), kind);
    }
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroUsize};
    use std::time::Duration;

    use super::super::super::error::{PlanErrorKind, ReadPlace, VarScope};
    use super::super::super::step::{
        Body, Comparison, Condition, ExtractSource, Extraction, IfStep, JsonAssertion,
        JsonTemplate, RepeatStep, RequestStep, RequestTarget, Scalar, Step,
    };
    use super::super::super::template::VarRef;
    use super::super::super::workload::PlanDraft;
    use super::super::testing::{draft, errors, request, scenario, tpl, wait};

    use VarScope::{BuiltIn, Global, User};

    const SCEN: VarScope = VarScope::Scenario;

    fn url() -> RequestTarget {
        RequestTarget::Url(tpl("http://localhost/"))
    }

    fn get() -> RequestStep {
        request("GET", url())
    }

    fn step(r: RequestStep) -> Step {
        Step::Request(Box::new(r))
    }

    fn extraction(name: &str) -> Extraction {
        Extraction {
            var: name.to_string(),
            source: ExtractSource::Header("X".to_string()),
        }
    }

    /// A request that extracts `name`.
    fn extracts(name: &str) -> Step {
        let mut r = get();
        r.extract = vec![extraction(name)];
        step(r)
    }

    /// A request that reads `text` in its header `H`.
    fn reads(text: &str) -> Step {
        let mut r = get();
        r.headers = vec![("H".to_string(), tpl(text))];
        step(r)
    }

    fn extracting(names: &[&str]) -> Vec<Step> {
        names.iter().map(|name| extracts(name)).collect()
    }

    fn if_then(then: Vec<Step>) -> Step {
        Step::If(IfStep {
            when: Condition::Compare {
                var: VarRef::VuId,
                op: Comparison::Exists(true),
            },
            then,
            otherwise: vec![],
        })
    }

    fn repeat(steps: Vec<Step>) -> Step {
        Step::Repeat(RepeatStep {
            count: NonZeroU32::new(1).unwrap(),
            deadline: Duration::from_secs(60),
            steps,
        })
    }

    fn not_set(scope: VarScope, hint: Option<VarScope>) -> PlanErrorKind {
        PlanErrorKind::VariableNotSet {
            scope,
            hint,
            position: None,
        }
    }

    fn not_readable(scope: VarScope, place: ReadPlace, hint: Option<VarScope>) -> PlanErrorKind {
        PlanErrorKind::VariableNotReadable {
            scope,
            place,
            hint,
            position: None,
        }
    }

    fn at(path: &str, kind: PlanErrorKind) -> (String, PlanErrorKind) {
        (path.to_string(), kind)
    }

    const REQ: &str = "scenarios[0].steps[0].request";

    /// The places the scope table reads from.
    #[derive(Debug, Clone, Copy)]
    enum Where {
        BaseUrl,
        FileSetup,
        FileTeardown,
        ScenarioSetup,
        ScenarioTeardown,
        VuSetup,
        Steps,
        VuTeardown,
    }

    /// Every place extracts its own name: `g` and `gt` at file level, and `s`,
    /// `st` and `u` in the scenario.
    fn table_draft() -> PlanDraft {
        let mut s = scenario("a");
        s.setup = extracting(&["s"]);
        s.teardown = extracting(&["st"]);
        s.vu_setup = extracting(&["u"]);
        let mut d = draft(vec![s]);
        d.setup = extracting(&["g"]);
        d.teardown = extracting(&["gt"]);
        d
    }

    /// Adds a read of `text` at `place`, after that place's own `extract`, and
    /// returns the path its errors go at.
    fn read_at(d: &mut PlanDraft, place: Where, text: &str) -> String {
        fn add(list: &mut Vec<Step>, prefix: &str, text: &str) -> String {
            list.push(reads(text));
            format!("{prefix}[{}].request.headers.H", list.len() - 1)
        }
        let s = &mut d.scenarios[0];
        match place {
            Where::BaseUrl => {
                d.base_url = Some(tpl(text));
                "base_url".to_string()
            }
            Where::FileSetup => add(&mut d.setup, "setup", text),
            Where::FileTeardown => add(&mut d.teardown, "teardown", text),
            Where::ScenarioSetup => add(&mut s.setup, "scenarios[0].setup", text),
            Where::ScenarioTeardown => add(&mut s.teardown, "scenarios[0].teardown", text),
            Where::VuSetup => add(&mut s.vu_setup, "scenarios[0].vu_setup", text),
            Where::Steps => add(&mut s.steps, "scenarios[0].steps", text),
            Where::VuTeardown => add(&mut s.vu_teardown, "scenarios[0].vu_teardown", text),
        }
    }

    #[test]
    fn reads_follow_the_scope_table() {
        use ReadPlace as R;
        assert!(errors(&table_draft()).is_empty());
        let reads = [
            "${u}",
            "${vu.id}",
            "${scenario.s}",
            "${scenario.st}",
            "${global.g}",
            "${global.gt}",
            "${env.E}",
        ];
        let no = |scope, place| Some(not_readable(scope, place, None));
        let unset = |scope| Some(not_set(scope, None));
        let file = |place, gt| {
            [
                no(User, place),
                no(BuiltIn, place),
                no(SCEN, place),
                no(SCEN, place),
                None,
                gt,
                None,
            ]
        };
        let scenario_hook = |place, st| {
            [
                no(User, place),
                no(BuiltIn, place),
                None,
                st,
                None,
                unset(Global),
                None,
            ]
        };
        let user = [None, None, None, unset(SCEN), None, unset(Global), None];
        let table = [
            (
                Where::BaseUrl,
                [
                    no(User, R::BaseUrl),
                    no(BuiltIn, R::BaseUrl),
                    no(SCEN, R::BaseUrl),
                    no(SCEN, R::BaseUrl),
                    no(Global, R::BaseUrl),
                    no(Global, R::BaseUrl),
                    None,
                ],
            ),
            (Where::FileSetup, file(R::FileSetup, unset(Global))),
            (Where::FileTeardown, file(R::FileTeardown, None)),
            (
                Where::ScenarioSetup,
                scenario_hook(R::ScenarioSetup, unset(SCEN)),
            ),
            (
                Where::ScenarioTeardown,
                scenario_hook(R::ScenarioTeardown, None),
            ),
            (Where::VuSetup, user.clone()),
            (Where::Steps, user.clone()),
            (Where::VuTeardown, user),
        ];
        for (place, row) in table {
            for (text, want) in reads.into_iter().zip(row) {
                let mut d = table_draft();
                let path = read_at(&mut d, place, text);
                let want: Vec<_> = want.into_iter().map(|kind| (path.clone(), kind)).collect();
                assert_eq!(errors(&d), want, "{text} at {place:?}");
            }
        }
    }

    /// A draft whose `steps` read `text`, with `extract`s in the file setup,
    /// the scenario setup and `vu_setup`.
    fn steps_read(text: &str, file_setup: &[&str], setup: &[&str], vu_setup: &[&str]) -> PlanDraft {
        let mut s = scenario("a");
        s.setup = extracting(setup);
        s.vu_setup = extracting(vu_setup);
        s.steps = vec![reads(text)];
        let mut d = draft(vec![s]);
        d.setup = extracting(file_setup);
        d
    }

    #[test]
    fn hints_name_a_scope_that_would_work() {
        let header = format!("{REQ}.headers.H");

        let mut d = draft(vec![scenario("a")]);
        d.setup = extracting(&["admin_token"]);
        d.teardown = vec![reads("${admin_token}")];
        assert_eq!(
            errors(&d),
            [at(
                "teardown[0].request.headers.H",
                not_readable(User, ReadPlace::FileTeardown, Some(Global))
            )]
        );

        let cases = [
            (
                steps_read("${global.token}", &[], &[], &["token"]),
                not_set(Global, Some(User)),
            ),
            (
                steps_read("${currency}", &[], &["currency"], &[]),
                not_set(User, Some(SCEN)),
            ),
            (
                steps_read("${x}", &["x"], &["x"], &[]),
                not_set(User, Some(SCEN)),
            ),
            (
                steps_read("${global.x}", &[], &["x"], &["x"]),
                not_set(Global, Some(User)),
            ),
        ];
        for (d, want) in cases {
            assert_eq!(errors(&d), [at(&header, want)]);
        }

        let mut s = scenario("a");
        s.setup = vec![reads("${token}")];
        s.vu_setup = extracting(&["token"]);
        assert_eq!(
            errors(&draft(vec![s])),
            [at(
                "scenarios[0].setup[0].request.headers.H",
                not_readable(User, ReadPlace::ScenarioSetup, None)
            )]
        );
    }

    #[test]
    fn positions_count_only_real_references() {
        let mut r = get();
        r.headers = ["Bearer ${a} and ${b}", "x ${b}", "$${a} ${b}", "$${nope}"]
            .iter()
            .enumerate()
            .map(|(i, text)| (format!("H{i}"), tpl(text)))
            .collect();
        let mut s = scenario("a");
        s.vu_setup = extracting(&["a"]);
        s.steps = vec![step(r)];
        let header = |i: usize| format!("{REQ}.headers.H{i}");
        assert_eq!(
            errors(&draft(vec![s])),
            [
                (
                    header(0),
                    PlanErrorKind::VariableNotSet {
                        scope: User,
                        hint: None,
                        position: NonZeroUsize::new(2),
                    }
                ),
                (header(1), not_set(User, None)),
                (header(2), not_set(User, None)),
            ]
        );
    }

    #[test]
    fn every_template_place_is_checked() {
        let nope = || tpl("${nope}");
        let assertion = |op| JsonAssertion {
            path: "$.a".parse().unwrap(),
            op,
        };
        let mut first = request("GET", RequestTarget::Path(tpl("/${nope}")));
        first.query = vec![("q".to_string(), nope())];
        first.headers = vec![("H".to_string(), nope())];
        first.body = Some(Body::Text(nope()));
        first.expect.body_contains = Some(nope());
        first.expect.json = vec![
            assertion(Comparison::Equals(Scalar::String(nope()))),
            assertion(Comparison::NotEquals(Scalar::String(nope()))),
            assertion(Comparison::Contains(nope())),
        ];
        let mut second = request("POST", RequestTarget::Url(tpl("http://localhost/${nope}")));
        second.body = Some(Body::Json(JsonTemplate::Object(vec![(
            "a".to_string(),
            JsonTemplate::Array(vec![JsonTemplate::Object(vec![(
                "b".to_string(),
                JsonTemplate::String(nope()),
            )])]),
        )])));
        let mut third = request("POST", url());
        third.body = Some(Body::Form(vec![("f".to_string(), nope())]));
        let when = Condition::All(vec![
            Condition::Compare {
                var: VarRef::Vu("nope".to_string()),
                op: Comparison::Exists(true),
            },
            Condition::Compare {
                var: VarRef::VuId,
                op: Comparison::Equals(Scalar::String(nope())),
            },
            Condition::Not(Box::new(Condition::Compare {
                var: VarRef::VuId,
                op: Comparison::Contains(nope()),
            })),
        ]);
        let mut s = scenario("a");
        s.steps = vec![
            step(first),
            step(second),
            step(third),
            Step::If(IfStep {
                when,
                then: vec![wait()],
                otherwise: vec![],
            }),
        ];
        let mut d = draft(vec![s]);
        d.base_url = Some(tpl("http://localhost"));
        let want: Vec<_> = [
            "steps[0].request.path",
            "steps[0].request.query.q",
            "steps[0].request.headers.H",
            "steps[0].request.body",
            "steps[0].request.expect.body_contains",
            "steps[0].request.expect.json[0].equals",
            "steps[0].request.expect.json[1].not_equals",
            "steps[0].request.expect.json[2].contains",
            "steps[1].request.url",
            "steps[1].request.json.a[0].b",
            "steps[2].request.form.f",
            "steps[3].if.when.all[0].var",
            "steps[3].if.when.all[1].equals",
            "steps[3].if.when.all[2].not.contains",
        ]
        .iter()
        .map(|path| at(&format!("scenarios[0].{path}"), not_set(User, None)))
        .collect();
        assert_eq!(errors(&d), want);
    }

    #[test]
    fn nested_extracts_and_reads_are_found() {
        let mut s = scenario("a");
        s.vu_setup = vec![if_then(vec![repeat(extracting(&["t"]))])];
        s.steps = vec![
            reads("${t}"),
            repeat(vec![if_then(vec![reads("${missing}")])]),
        ];
        assert_eq!(
            errors(&draft(vec![s])),
            [at(
                "scenarios[0].steps[1].repeat.steps[0].if.then[0].request.headers.H",
                not_set(User, None)
            )]
        );
    }

    #[test]
    fn extract_names_must_be_valid_variable_names() {
        let names = [
            "x", "_x1", "1x", "my-var", "", "env", "global", "scenario", "vu",
        ];
        let mut r = get();
        r.extract = names.iter().map(|name| extraction(name)).collect();
        let mut s = scenario("a");
        // A name that isn't valid isn't set, even when a reference names it.
        let compare = |name: &str| Condition::Compare {
            var: VarRef::Vu(name.to_string()),
            op: Comparison::Exists(true),
        };
        s.steps = vec![
            step(r),
            Step::If(IfStep {
                when: Condition::All(vec![compare("x"), compare("my-var")]),
                then: vec![wait()],
                otherwise: vec![],
            }),
        ];
        let name = |name: &str| format!("{REQ}.extract.{name}");
        let invalid = PlanErrorKind::InvalidVariableName;
        let reserved = PlanErrorKind::ReservedVariableName;
        assert_eq!(
            errors(&draft(vec![s])),
            [
                at(&name("1x"), invalid.clone()),
                at(&name("my-var"), invalid.clone()),
                at(&name(""), invalid),
                at(&name("env"), reserved.clone()),
                at(&name("global"), reserved.clone()),
                at(&name("scenario"), reserved.clone()),
                at(&name("vu"), reserved),
                at(
                    "scenarios[0].steps[1].if.when.all[1].var",
                    not_set(User, None)
                ),
            ]
        );
    }

    #[test]
    fn base_url_reads_only_env() {
        let with_base_url = |text: &str| {
            let mut d = draft(vec![scenario("a")]);
            d.setup = extracting(&["g"]);
            d.base_url = Some(tpl(text));
            d
        };
        assert!(errors(&with_base_url("${env.X}")).is_empty());
        assert_eq!(
            errors(&with_base_url("${global.g}")),
            [at(
                "base_url",
                not_readable(Global, ReadPlace::BaseUrl, None)
            )]
        );
    }

    #[test]
    fn messages_read_as_written() {
        assert_eq!(
            PlanErrorKind::VariableNotSet {
                scope: User,
                hint: Some(SCEN),
                position: NonZeroUsize::new(2),
            }
            .to_string(),
            "no `extract` in this scenario's `vu_setup`, `steps` or `vu_teardown` sets this \
             variable; it's set in this scenario's hooks, so read it with `scenario.` \
             (reference 2 in this value)"
        );
        assert_eq!(
            PlanErrorKind::VariableNotReadable {
                scope: BuiltIn,
                place: ReadPlace::ScenarioSetup,
                hint: None,
                position: None,
            }
            .to_string(),
            "`vu.id` and `vu.iteration` can't be read here, because a scenario's setup runs \
             once, before its users start"
        );
        assert_eq!(
            not_readable(Global, ReadPlace::BaseUrl, None).to_string(),
            "`base_url` can only read `env.` variables, because it's used before any setup runs"
        );
    }
}
